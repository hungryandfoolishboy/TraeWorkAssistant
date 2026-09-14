//! Rust 版 MITM 代理（device_proxy.py 迁移，P4）。
//! 架构：hyper 1.x 协议栈自建（hudsucker 非拦截 CONNECT 隧道硬编码直连、无法透传用户 VPN 上游）。
//! 模块：ca 证书签发 / logger 请求日志 / upstream 上游连接 / handler MITM 改写 / ws 桥接。
//! 本文件：代理生命周期（ProxyServer）+ 主循环（accept → CONNECT 分流 / 明文转发）。

pub mod ca;
pub mod handler;
pub mod local_capture;
pub mod logger;
pub mod upstream;
pub mod ws;
pub mod bypass;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{Full, Limited};
use hyper::body::Bytes;
use hyper::header::{HeaderName, HeaderValue};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::device_proxy::ca::{ensure_ca, CaAuthority};
use crate::device_proxy::handler::{serve_mitm, HOP_BY_HOP_REQ, ProxyCtx};
use crate::device_proxy::logger::{ProxyLog, RequestLogger};
use crate::device_proxy::upstream::{
    connect_direct, connect_via_upstream, UpstreamConnector, UpstreamProxy,
};

/// 建连/首读超时（对齐 Python `_CONN_TIMEOUT`）
const CONN_TIMEOUT: Duration = Duration::from_secs(300);
/// 明文转发上游超时（对齐 Python handle_plain 的 socket timeout=30s）
const PLAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 分发阶段头缓冲上限（Python 无上限仅靠超时兜底，此处防御性 64KB）
const MAX_DISPATCH_HEAD: usize = 64 * 1024;
/// 并发连接上限（对齐 Python `_CONN_SEMAPHORE` 信号量 128）
const MAX_CONNS: usize = 128;
/// CONNECT 200 应答后的 TLS 握手超时（客户端不发 ClientHello 时及时释放连接与并发槽）
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认监听域名（对齐 Python `TARGET_DOMAINS`；桌面端设置页 PROXY_DOMAINS 可覆盖）
pub const DEFAULT_TARGETS: &[&str] = &[
    "trae.cn",
    "trae.com.cn",
    "mchost.guru",
    "zijieapi.com",
    "bytedance.com",
    "volcengine.com",
    "volces.com",
    "treecode.com",
    "doubao.com",
];

// ---------------- 生命周期 ----------------

/// 代理启动配置（由 commands/proxy.rs 从 AppState / 设置页构造）
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// 监听端口（恒为 127.0.0.1）
    pub port: u16,
    /// 监听域名列表（后缀匹配，构造时统一小写；空则用 [`DEFAULT_TARGETS`]）
    pub targets: Vec<String>,
    /// 自动捕获 JWT 写回 accounts（AUTO_CAPTURE_JWT，默认开）
    pub auto_capture_jwt: bool,
    /// data/checkin_accounts.json
    pub accounts_path: PathBuf,
    /// data/account_cooldowns.json
    pub cooldowns_path: PathBuf,
    /// data/doubao_captured_credentials.json
    pub doubao_cred_path: PathBuf,
    /// data/certs（CA 目录，与 Python 版布局一致）
    pub certs_dir: PathBuf,
    /// logs/proxy.log 操作日志
    pub log_path: PathBuf,
    /// 代理请求抓包日志目录（按日滚动 + 100MB 切分）
    pub req_log_dir: PathBuf,
    /// 上游代理（用户 VPN 梯子；非目标流量经此转出，失败回退直连）
    pub upstream: Option<UpstreamProxy>,
}

/// 运行中的代理句柄。drop shutdown 发送端即触发停止（changed() 出错分支），
/// 但建议显式调用 [`ProxyServer::stop`] 并按需 [`ProxyServer::join`] 等待端口释放。
pub struct ProxyServer {
    pub port: u16,
    pub captured: Arc<AtomicI64>,
    shutdown_tx: watch::Sender<bool>,
    exit_rx: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl ProxyServer {
    /// 启动进程内代理（对齐 Python `main()`：ensure_ca → 设备标识同步 → 独占绑定 → accept 循环）
    pub async fn start(cfg: ProxyConfig, app: Option<tauri::AppHandle>) -> Result<ProxyServer, String> {
        // CA 证书：兼容已有 Python 版 RSA CA；缺失则生成（数据目录布局不变）
        let ca = Arc::new(ensure_ca(&cfg.certs_dir)?);

        let captured = Arc::new(AtomicI64::new(0));
        let log = ProxyLog::new(cfg.log_path.clone(), app, Arc::clone(&captured));
        let req_logger = Arc::new(RequestLogger::new(cfg.req_log_dir.clone()));
        let targets = if cfg.targets.is_empty() {
            DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect()
        } else {
            cfg.targets.iter().map(|d| d.to_ascii_lowercase()).collect()
        };
        let ctx = Arc::new(ProxyCtx {
            log: log.clone(),
            req_logger,
            targets,
            auto_capture_jwt: cfg.auto_capture_jwt,
            accounts_path: cfg.accounts_path.clone(),
            cooldowns_path: cfg.cooldowns_path.clone(),
            doubao_cred_path: cfg.doubao_cred_path.clone(),
        });

        // 升级历史假占位符设备标识（对齐 Python sync_account_devices，仅自动捕获开启时）
        if ctx.auto_capture_jwt {
            sync_account_devices(&ctx);
        }

        // Windows 独占绑定（SO_EXCLUSIVEADDRUSE，issue #7：防孤儿进程「假启动」）
        let listener = bind_listener(cfg.port).await?;

        // 启动横幅（对齐 Python main() 的日志行，前端实时代理面板直接可读）
        log.log(&format!(
            "代理已启动: 127.0.0.1:{}  (TRAE 多域 MITM 拦截 + JWT 自动捕获)",
            cfg.port
        ));
        let list: Vec<String> = ctx.targets.iter().map(|d| format!("*.{d}")).collect();
        log.log(&format!("监听 TRAE 域名: {}", list.join(", ")));
        log.log("  → 命中上述域名的请求会在面板中以 [TRAE] 标记；JWT 捕获不限 host（兼容未列出的子域）");
        log.log("  → 未在监听域名列表中的请求将透明转发（不记录日志），不影响其他 App 正常上网");
        log.log(&format!("accounts: {}", cfg.accounts_path.display()));
        log.log(&format!("代理请求日志: {} (100MB 滚动)", cfg.req_log_dir.display()));
        log.log(&format!(
            "自动捕获 JWT 写回 accounts.json: {}",
            if cfg.auto_capture_jwt { "开" } else { "关" }
        ));
        if let Some(up) = &cfg.upstream {
            log.log(&format!("上游代理(用户VPN)透传: {}", up.addr()));
        }
        log.log("请把 CA 证书 certs/ca.cer 安装到 Windows 受信任根证书颁发机构(管理员)。");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // 退出通知：accept 循环结束（无论主动 stop 还是意外崩溃）时置 true，
        // 供 commands/proxy.rs 看门狗监听并还原系统代理（对齐 Python 版 stdout EOF 看门狗语义）
        let (exit_tx, exit_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            accept_loop(listener, ctx, ca, cfg.upstream.clone(), shutdown_rx).await;
            let _ = exit_tx.send(true);
        });
        Ok(ProxyServer { port: cfg.port, captured, shutdown_tx, exit_rx, task })
    }

    /// 主动停止：accept 循环退出并中止所有在途连接任务
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// 任务退出通知（主动 stop 与意外崩溃均会触发；调用方结合「主动停止」标记区分）
    pub fn exit_signal(&self) -> watch::Receiver<bool> {
        self.exit_rx.clone()
    }

    /// 代理任务是否仍在运行（意外崩溃时为 false，供看门狗判定）
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

// ---------------- 主循环 ----------------

async fn accept_loop(
    listener: TcpListener,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    mut shutdown: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNS));
    // 明文转发双 Client（路由对齐 Python handle_plain）：
    // - plain_client：非目标域名 http 请求经用户 VPN 上游（http 代理绝对形式 / SOCKS5 隧道，
    //   失败回退直连），无上游配置时即直连
    // - direct_client：目标域名 / https 明文请求一律直连（Trae 域国内可达，
    //   Python 版上游仅服务非目标域名 http，不把 TRAE 流量绕行用户梯子）
    let plain_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(upstream.clone(), ctx.log.clone()));
    let direct_client: Client<UpstreamConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(UpstreamConnector::new(None, ctx.log.clone()));
    let mut conns: Vec<JoinHandle<()>> = Vec::new();
    // 空闲期定时回收已结束的连接句柄（审查修复：conns 仅在新 accept 时清理，
    // 长连接高频场景下已完成任务的 JoinHandle 会随 Vec 无界增长）
    let mut reap = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            // stop() 或 ProxyServer 整体 drop（发送端析构 → changed() 报错）都会触发退出
            _ = shutdown.changed() => break,
            _ = reap.tick() => {
                conns.retain(|h| !h.is_finished());
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // 并发超限（try_acquire 失败）直接关闭新连接，保证代理自身不被打挂。
                    // permit 必须移入任务、持有至连接结束（审查修复：原 try_acquire()
                    // 临时值语句结束即析构，MAX_CONNS 上限完全失效、过载分支不可达）
                    let permit = match Arc::clone(&permits).try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            ctx.log.log(&format!(
                                "[overload] 并发连接已达上限 {MAX_CONNS}，拒绝来自 {peer} 的新连接"
                            ));
                            continue;
                        }
                    };
                    conns.retain(|h| !h.is_finished());
                    let task_ctx = Arc::clone(&ctx);
                    let task_ca = Arc::clone(&ca);
                    let task_up = upstream.clone();
                    let task_client = plain_client.clone();
                    let task_direct = direct_client.clone();
                    conns.push(tokio::spawn(async move {
                        let _guard = permit; // 释放即归还信号量
                        handle_conn(stream, peer, task_ctx, task_ca, task_up, task_client, task_direct).await;
                    }));
                }
                Err(e) => {
                    // 单条 accept 出错不应让整个代理退出（否则系统代理仍指向死端口）。
                    // 记录后短暂退避再重试，保持服务可用。
                    ctx.log.log(&format!("[accept] 异常(已忽略并重试): {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    // 停止：中止所有在途连接任务（对齐 Python「进程被杀」的停止语义）
    for h in conns {
        h.abort();
    }
    ctx.log.log("代理已停止");
}

/// 单连接分发（对齐 Python `handle_client`）：
/// - CONNECT + 目标域名 → 200 应答 → TLS(叶子证书) → [`serve_mitm`] 解密改写
/// - CONNECT + 其他域名 → [`tunnel_raw`] 透明隧道（不解密不记日志）
/// - 其余（明文 HTTP）→ [`handle_plain`] 转发
async fn handle_conn(
    mut stream: TcpStream,
    peer: SocketAddr,
    ctx: Arc<ProxyCtx>,
    ca: Arc<CaAuthority>,
    upstream: Option<UpstreamProxy>,
    plain_client: Client<UpstreamConnector, Full<Bytes>>,
    direct_client: Client<UpstreamConnector, Full<Bytes>>,
) {
    let head = match timeout(CONN_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            ctx.log.log(&format!("[client] {peer} 读头失败: {e}"));
            return;
        }
        Err(_) => {
            ctx.log.log(&format!("[client] {peer} 读头超时 ({}s)", CONN_TIMEOUT.as_secs()));
            return;
        }
    };
    let first = String::from_utf8_lossy(head.split(|&b| b == b'\n').next().unwrap_or(b""))
        .trim()
        .to_string();
    let method = first.split(' ').next().unwrap_or("").to_ascii_uppercase();
    if method == "CONNECT" {
        // CONNECT host:port HTTP/1.1
        let target = first.split(' ').nth(1).unwrap_or("");
        let (host, port) = match target.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(443)),
            None => (target.to_string(), 443),
        };
        // 审查修复：read_head 按块读会超读 head 之后的字节（客户端在 200 应答前
        // 抢发的 ClientHello / pipelined 数据），必须回放给后续 TLS 握手/隧道，
        // 否则握手从空流开始将挂死（明文路径已用 init 注入，此处此前被直接丢弃）
        let overflow = head_after_head_end(&head);
        let mut client = PrefixedStream::new(stream, overflow);
        if !ctx.host_in_targets(&host) {
            tunnel_raw(client, &host, port, &upstream, &ctx.log).await;
            return;
        }
        let matched = ctx
            .targets
            .iter()
            .find(|d| **d == host || host.ends_with(&format!(".{d}")))
            .map(|s| s.as_str())
            .unwrap_or("?");
        // 先握手后记日志（与 Python 一致）：避免日志/证书等任何异常把 CONNECT
        // 握手拖死导致客户端 EOF（issue #7）
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        ctx.log.log(&format!("CONNECT {host}:{port}  [TRAE/MITM] 匹配域名: {matched}"));
        let acceptor = TlsAcceptor::from(ca.gen_server_config(&host));
        // 握手超时（审查修复：原无超时，客户端不发 ClientHello 时任务永久挂起）
        match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(client)).await {
            Ok(Ok(tls)) => serve_mitm(tls, host, port, ctx).await,
            Ok(Err(e)) => ctx.log.log(&format!("  [MITM] TLS 握手失败 {host}:{port}: {e}")),
            Err(_) => ctx.log.log(&format!(
                "  [MITM] TLS 握手超时 ({}s) {host}:{port}",
                HANDSHAKE_TIMEOUT.as_secs()
            )),
        }
    } else {
        // 明文 HTTP 请求：全部转发（日志由 handle_plain 内部按目标域名控制）
        handle_plain(stream, head, &plain_client, &direct_client, &ctx).await;
    }
}

/// 分发阶段读请求头（到 \r\n\r\n 或 EOF；EOF 时返回已有内容交由上层判路由，对齐 Python）
async fn read_head<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if handler::find_head_end(&buf).is_some() {
            return Ok(buf);
        }
        let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_DISPATCH_HEAD {
            return Err(format!("请求头超过分发缓冲上限 ({MAX_DISPATCH_HEAD})"));
        }
    }
}

/// 非目标域名 CONNECT 的透明隧道（对齐 Python `tunnel_raw`）：
/// 上游代理（用户 VPN）优先，失败回退直连；全程不解密，仅记隧道级日志。
/// client 为 [`PrefixedStream`]（分发阶段超读字节的回放见 handle_conn 注释）。
async fn tunnel_raw<S>(
    mut client: S,
    host: &str,
    port: u16,
    upstream: &Option<UpstreamProxy>,
    log: &ProxyLog,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pre: Option<TcpStream> = None;
    if let Some(up) = upstream {
        match connect_via_upstream(host, port, up).await {
            Ok(s) => {
                log.log(&format!("  [raw-tunnel] 经上游代理 {} 建立隧道 {host}:{port}", up.addr()));
                pre = Some(s);
            }
            Err(e) => log.log(&format!("  [raw-tunnel] 上游代理连接失败({e})，回退直连")),
        }
    }
    let mut remote = match pre {
        Some(r) => r,
        None => match connect_direct(host, port).await {
            Ok(s) => s,
            Err(_) => {
                // 上游不可达：明确告知客户端，避免浏览器无限等待
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            }
        },
    };
    // 完成 CONNECT 握手：先回 200，客户端随后才会发送 TLS ClientHello
    if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
        return;
    }
    // 双向裸转发（copy_bidirectional 自带半关闭传播，对齐 Python pipe + shutdown(SHUT_WR)）
    let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await;
}

/// 明文 HTTP 转发（对齐 Python `handle_plain`）：
/// 请求行为代理形式绝对 URL；仅「非目标域名 + http」经上游 VPN 转发（hyper 绝对形式），
/// 目标域名 / https 明文请求一律直连（Python 版同款路由，不把 TRAE 流量绕行用户梯子）。
/// 仅目标域名记操作日志与抓包日志；单请求后关闭连接（对齐 Python 语义）。
async fn handle_plain(
    mut stream: TcpStream,
    head: Vec<u8>,
    plain_client: &Client<UpstreamConnector, Full<Bytes>>,
    direct_client: &Client<UpstreamConnector, Full<Bytes>>,
    ctx: &ProxyCtx,
) {
    // 分发阶段已读取的字节作为初始缓冲注入（可能含 body 前缀）
    let init = bytes::BytesMut::from(&head[..]);
    let Some(req) = (match timeout(CONN_TIMEOUT, handler::read_raw_request_buf(&mut stream, init)).await {
        Ok(Ok(Some(r))) => Some(r),
        _ => None,
    }) else {
        return;
    };

    let Ok(uri) = req.path.parse::<hyper::Uri>() else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let Some(host) = uri.host().map(str::to_string) else {
        let _ = handler::send_response(&mut stream, 400, "Bad Request", &[], b"Bad Request").await;
        return;
    };
    let scheme = uri.scheme_str().unwrap_or("http").to_string();
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port = uri.port_u16().unwrap_or(default_port);
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let is_target = ctx.host_in_targets(&host);
    if is_target {
        ctx.log.log(&format!("  [plain] {} {scheme}://{host}:{port}{path}", req.method));
    }
    // 上游路由对齐 Python：仅「非目标域名 + http」经用户 VPN（失败回退直连）；
    // 目标域名 / https 明文请求直连
    let client = if !is_target && scheme == "http" { plain_client } else { direct_client };

    // 组装上游请求：过滤跳过头（host/content-length 由 hyper 依 URI/body 重写）
    let mut builder = Request::builder().method(req.method.as_str()).uri(uri.clone());
    for (k, v) in &req.headers {
        if HOP_BY_HOP_REQ.iter().any(|h| k.eq_ignore_ascii_case(h)) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), v.parse::<HeaderValue>()) {
            builder = builder.header(name, val);
        }
    }
    // Python：GET 请求不带 body
    let body = if req.method.eq_ignore_ascii_case("GET") { Bytes::new() } else { req.body.clone() };
    let request = builder.body(Full::new(body)).expect("plain upstream request build");

    let resp = match timeout(PLAIN_TIMEOUT, client.request(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            if is_target {
                ctx.log.log(&format!("  [plain] 错误: {e} (host={host}, path={path})"));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
            return;
        }
        Err(_) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] 错误: 上游超时 ({}s) (host={host}, path={path})",
                    PLAIN_TIMEOUT.as_secs()
                ));
            }
            let _ = handler::send_response(&mut stream, 504, "Gateway Timeout", &[], b"Gateway Timeout").await;
            return;
        }
    };

    let status = resp.status().as_u16();
    let reason = handler::reason_phrase(status);
    let resp_pairs: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    // 非流式整体缓冲：Limited 上限 + 逐帧空闲超时（差异修复：对齐 Python handle_plain
    // 30s socket timeout 的逐读语义，防 trickling body 挂住连接）
    match crate::device_proxy::handler::collect_body_with_idle_timeout(
        Limited::new(resp.into_body(), crate::device_proxy::handler::MAX_RESP_BODY),
        PLAIN_TIMEOUT,
    )
    .await
    {
        Ok(resp_body) => {
            if is_target {
                ctx.log.log(&format!(
                    "  [plain] <- {status} {reason} ({}) bytes from {host}{path}",
                    resp_body.len()
                ));
            }
            if handler::send_response(&mut stream, status, reason, &resp_pairs, &resp_body)
                .await
                .is_err()
            {
                return;
            }
            // 仅目标域名记录到代理请求日志（对齐 Python handle_plain）
            if is_target {
                ctx.req_logger.log_request(
                    &req.method,
                    &host,
                    &path,
                    &req.headers,
                    &req.body,
                    status,
                    reason,
                    &resp_pairs,
                    &resp_body,
                );
            }
        }
        Err(e) => {
            if is_target {
                ctx.log.log(&format!("  [plain] 错误: 读上游响应失败: {e} (host={host}, path={path})"));
            }
            let _ = handler::send_response(&mut stream, 502, "Bad Gateway", &[], b"Bad Gateway").await;
        }
    }
}

// ---------------- 启动辅助 ----------------

/// 提取请求头之后超读的字节（客户端在 CONNECT 200 应答前抢发的 ClientHello /
/// pipelined 数据），供 [`PrefixedStream`] 回放给后续 TLS 握手/隧道
fn head_after_head_end(head: &[u8]) -> Vec<u8> {
    match handler::find_head_end(head) {
        Some(pos) => head[pos + 4..].to_vec(),
        None => Vec::new(),
    }
}

/// 带前缀缓冲的流：读操作先耗尽前缀（分发阶段超读字节的回放）再透传底层流，
/// 写操作直接透传。审查修复：此前超读字节被直接丢弃，TLS 握手从空流开始会挂死。
struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    pos: usize,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self { inner, prefix, pos: 0 }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let n = (this.prefix.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.prefix[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 把 checkin_accounts.json 中各账号的 device 字段刷新为当前算法生成的设备标识
///（对齐 Python `sync_account_devices`：升级历史记录中由旧算法生成的假占位符
/// device_id 全 '2' / session_id 全 '5'；仅当字段确实变化时才写盘）
pub fn sync_account_devices(ctx: &ProxyCtx) {
    let _g = handler::accounts_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg: serde_json::Value = crate::fs_utils::read_json(&ctx.accounts_path);
    if !cfg.get("accounts").map(|a| a.is_array()).unwrap_or(false) {
        return;
    }
    let accounts = cfg
        .get_mut("accounts")
        .and_then(|a| a.as_array_mut())
        .expect("accounts array");
    let mut changed = false;
    for a in accounts.iter_mut() {
        let uid = a
            .get("UserID")
            .or_else(|| a.get("user_id"))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty());
        let Some(uid) = uid else { continue };
        let dev = crate::commands::accounts::derive_device(&uid);
        let same = a.get("device_id").and_then(|v| v.as_str()) == Some(dev.device_id.as_str())
            && a.get("session_id").and_then(|v| v.as_str()) == dev.session_id.as_deref()
            && a.get("market_user_id").and_then(|v| v.as_str()) == dev.market_user_id.as_deref();
        if same {
            continue;
        }
        a["device_id"] = serde_json::Value::String(dev.device_id.clone());
        a["session_id"] = dev
            .session_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        a["market_user_id"] = dev
            .market_user_id
            .clone()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        changed = true;
    }
    if changed {
        if crate::fs_utils::write_json(&ctx.accounts_path, &cfg).is_ok() {
            ctx.log.log("  [sync] 已刷新 accounts.json 中设备标识字段(旧算法升级)");
        } else {
            ctx.log.log("  [sync] accounts.json 写入失败（设备标识刷新未落盘）");
        }
    }
}

/// Windows：WSASocketW + SO_EXCLUSIVEADDRUSE 独占绑定（选项必须在 bind 前设置，
/// 对齐 Python `srv.setsockopt(SOL_SOCKET, SO_EXCLUSIVEADDRUSE, 1)`，issue #7）；
/// 其他平台：常规绑定（不设 SO_REUSEADDR，同样拒绝同端口重复绑定）。
fn bind_exclusive(port: u16) -> Result<std::net::TcpListener, String> {
    #[cfg(target_os = "windows")]
    unsafe {
        use std::os::windows::io::FromRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            bind as ws_bind, closesocket, listen as ws_listen, setsockopt, WSAGetLastError, WSASocketW,
            AF_INET, IN_ADDR, IN_ADDR_0, IN_ADDR_0_0, INVALID_SOCKET, IPPROTO_TCP, SOCKADDR, SOCKADDR_IN,
            SOCK_STREAM, SO_EXCLUSIVEADDRUSE, SOL_SOCKET, WSA_FLAG_OVERLAPPED,
        };
        let sock = WSASocketW(
            AF_INET as i32,
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED,
        );
        if sock == INVALID_SOCKET {
            return Err(format!("创建监听 socket 失败: WSA错误 {}", WSAGetLastError()));
        }
        // 独占绑定：多个 socket 绑定同一端口将明确失败（Python 版同款修复）
        let on: u32 = 1;
        if setsockopt(
            sock,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &on as *const u32 as *const u8,
            std::mem::size_of::<u32>() as i32,
        ) != 0
        {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("设置 SO_EXCLUSIVEADDRUSE 失败: WSA错误 {err}"));
        }
        let addr = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: port.to_be(),
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_un_b: IN_ADDR_0_0 { s_b1: 127, s_b2: 0, s_b3: 0, s_b4: 1 },
                },
            },
            sin_zero: [0; 8],
        };
        if ws_bind(
            sock,
            &addr as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("绑定 127.0.0.1:{port} 失败: WSA错误 {err}"));
        }
        if ws_listen(sock, 128) != 0 {
            let err = WSAGetLastError();
            closesocket(sock);
            return Err(format!("listen 失败: WSA错误 {err}"));
        }
        // fd 所有权转交 std（后续转 tokio 异步轮询）
        Ok(std::net::TcpListener::from_raw_socket(sock as u64))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::net::TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("绑定 127.0.0.1:{port} 失败: {e}"))
    }
}

async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    let std_listener = bind_exclusive(port)?;
    TcpListener::from_std(std_listener).map_err(|e| format!("监听器初始化失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aiwork_proxy_test_{}_{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn test_ctx(dir: &PathBuf) -> ProxyCtx {
        let captured = Arc::new(AtomicI64::new(0));
        ProxyCtx {
            log: ProxyLog::new(dir.join("proxy.log"), None, captured),
            req_logger: Arc::new(RequestLogger::new(dir.clone())),
            targets: DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect(),
            auto_capture_jwt: true,
            accounts_path: dir.join("checkin_accounts.json"),
            cooldowns_path: dir.join("account_cooldowns.json"),
            doubao_cred_path: dir.join("doubao_captured_credentials.json"),
        }
    }

    /// 假占位符（旧算法）设备字段应被刷新为当前算法值，且二次同步幂等
    #[test]
    fn sync_refreshes_placeholder_devices() {
        let dir = temp_dir("sync");
        let ctx = test_ctx(&dir);
        serde_json::to_writer(
            std::fs::File::create(&ctx.accounts_path).unwrap(),
            &serde_json::json!({
                "accounts": [
                    {"name": "a", "UserID": "4487568582777872", "device_id": "222222222222222",
                     "session_id": "55555555555555555555555555555555"},
                    {"name": "b", "user_id": 12345, "device_id": "222222222222222"}
                ]
            }),
        )
        .unwrap();
        sync_account_devices(&ctx);
        let cfg: serde_json::Value = crate::fs_utils::read_json(&ctx.accounts_path);
        let acc = &cfg["accounts"];
        let dev = crate::commands::accounts::derive_device("4487568582777872");
        assert_eq!(acc[0]["device_id"].as_str().unwrap(), dev.device_id);
        assert_eq!(acc[0]["session_id"].as_str(), dev.session_id.as_deref());
        assert_eq!(
            acc[1]["device_id"].as_str().unwrap(),
            crate::commands::accounts::derive_device("12345").device_id
        );
        // 二次同步应无变化（不触发写盘，文件内容逐字节一致）
        let before = std::fs::read_to_string(&ctx.accounts_path).unwrap();
        sync_account_devices(&ctx);
        let after = std::fs::read_to_string(&ctx.accounts_path).unwrap();
        assert_eq!(before, after);
    }

    /// 缺 accounts 数组 / 空文件时静默返回，不报错不写盘
    #[test]
    fn sync_tolerates_missing_accounts() {
        let dir = temp_dir("sync_empty");
        let ctx = test_ctx(&dir);
        sync_account_devices(&ctx); // 文件不存在
        std::fs::write(&ctx.accounts_path, "{}").unwrap();
        sync_account_devices(&ctx); // 空 accounts
        assert!(dir.join("checkin_accounts.json").exists());
    }

    /// Windows 独占绑定：同端口二次 bind 必须失败（issue #7 防孤儿进程假启动）
    #[cfg(target_os = "windows")]
    #[test]
    fn exclusive_bind_rejects_double_bind() {
        // 测试进程可能尚未初始化 WinSock（WSASocketW 需 WSAStartup，否则 10093）：
        // 先建一个 std 套接字触发进程级初始化，再测独占绑定
        drop(std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let first = bind_exclusive(0).expect("首次绑定(临时端口)应成功");
        let port = first.local_addr().unwrap().port();
        assert!(bind_exclusive(port).is_err(), "同端口二次绑定应失败");
    }

    /// head 之后的超读字节必须完整提取（空 head / 无超读 / 带超读三态）
    #[test]
    fn head_after_head_end_extracts_overflow() {
        assert_eq!(head_after_head_end(b"CONNECT a.com:443 HTTP/1.1\r\n\r\n"), b"");
        assert_eq!(
            head_after_head_end(b"GET / HTTP/1.1\r\nHost: a\r\n\r\nEXTRA-BYTES"),
            b"EXTRA-BYTES"
        );
        assert_eq!(head_after_head_end(b"partial-no-head-end"), b"");
    }

    /// PrefixedStream：先耗尽前缀（超读字节回放）再透传底层流（审查修复回归）
    #[tokio::test]
    async fn prefixed_stream_replays_overflow_then_inner() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(b"flow").await.unwrap();
        let mut s = PrefixedStream::new(server, b"over".to_vec());
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"over", "前缀字节优先回放");
        let n = s.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"flow", "前缀耗尽后透传底层流");
    }
}
