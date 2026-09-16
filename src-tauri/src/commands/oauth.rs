use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::{DeviceMap, RawAccount};
use crate::state::AppState;

/// 最近签发的 OAuth 登录会话（CSRF 防护 + PKCE）：oauth_get_login_url 签发时记录，
/// oauth_parse_callback 用 loginTraceID 双向绑定校验（抓包实证：login_trace_id 参数
/// 会被授权页原样回传为回调的 loginTraceID），PKCE verifier 供 AuthCode 交换
struct PendingLogin {
    state: String,
    pkce_verifier: String,
}
static LAST_OAUTH_STATE: Mutex<Option<PendingLogin>> = Mutex::new(None);

/// OAuth 常量（2026-09-16 抓包固化）：client_id 取真实 Trae IDE 登录 URL 实证值
/// （授权页 native_ide 流程 GetPCAuthCode 接受；旧值 en1oxy7wnw8j9n 会让页面
/// 停留在 billing status 后无后续，不回跳）。conf/oauth_client.json 可覆盖。
const OAUTH_CLIENT_ID: &str = "ono9krqynydwx5";
const OAUTH_CLIENT_SECRET: &str = "-";
/// 真实 IDE 页面参数快照（抓包 2026-09-16）：授权页据此进入 native_ide 原生授权
/// 流程（前端调 GetPCAuthCode 后 302 回 auth_callback_url）
const OAUTH_PAGE_PLUGIN_VERSION: &str = "2.3.83560";
const OAUTH_PAGE_APP_VERSION: &str = "3.3.100";
const OAUTH_PAGE_PLATFORM_CODE: &str = "IDE_PC";
/// 本机回环监听端口（F-78 批次 1：commands/oauth_loopback.rs 在此端口收 OAuth 回调）
pub(crate) const OAUTH_LOOPBACK_PORT: u16 = 17388;
pub(crate) const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";
const OAUTH_EXCHANGE_URL: &str = "https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken";

/// OAuth 客户端凭证外置配置（缺陷10）：conf/oauth_client.json 可覆盖
/// client_id / client_secret / exchange_url（上游更换凭证或端点时无需发版）。
/// 文件缺失或字段缺省回退内置默认；进程内 OnceLock 缓存，修改后需重启应用生效。
#[derive(serde::Deserialize, Clone)]
pub struct OAuthClientConfig {
    #[serde(default = "default_client_id")]
    pub client_id: String,
    #[serde(default = "default_client_secret")]
    pub client_secret: String,
    #[serde(default = "default_exchange_url")]
    pub exchange_url: String,
}

impl Default for OAuthClientConfig {
    fn default() -> Self {
        Self {
            client_id: default_client_id(),
            client_secret: default_client_secret(),
            exchange_url: default_exchange_url(),
        }
    }
}

fn default_client_id() -> String {
    OAUTH_CLIENT_ID.to_string()
}
fn default_client_secret() -> String {
    OAUTH_CLIENT_SECRET.to_string()
}
fn default_exchange_url() -> String {
    OAUTH_EXCHANGE_URL.to_string()
}

/// 读取外置 OAuth 客户端配置（全局一次；缺失/损坏回退默认值）
pub fn oauth_client() -> &'static OAuthClientConfig {
    static CFG: std::sync::OnceLock<OAuthClientConfig> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        std::env::var("APPDATA")
            .ok()
            .map(|d| {
                std::path::PathBuf::from(d)
                    .join(crate::state::DATA_DIR_NAME)
                    .join("conf")
                    .join("oauth_client.json")
            })
            .map(|p| fs_utils::read_json::<OAuthClientConfig>(&p))
            .unwrap_or_default()
    })
}

/// OAuth 回调解析结果
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OAuthCallbackInfo {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub avatar: Option<String>,
}

/// OAuth 登录 URL 响应
#[derive(Serialize)]
pub struct OAuthLoginUrl {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
}

/// OAuth 登录完成后的账号信息
#[derive(Serialize)]
pub struct OAuthLoginResult {
    pub user_id: String,
    pub name: String,
    pub jwt: String,
    pub refresh_token: String,
    pub has_refresh_token: bool,
}

/// 短请求 Agent（项目未启用 ureq 的 proxy-from-env feature，Agent 默认直连）
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

/// 交换/用户信息请求 Agent（F-78 批次 3 抓包调试）：
/// 默认与 short_agent 同款直连；设置环境变量 AIWORK_OAUTH_DEBUG_PROXY（值如
/// `127.0.0.1:8899`，即本软件 MITM 代理端口）时改走该代理并信任本地 CA
/// （%APPDATA%/AIWorkAssistant/certs/ca.crt，目录解析与 state.rs::new 同源），
/// ExchangeToken/GetUserInfo 流量落入代理日志（api.trae.com.cn 已默认入抓包域名），
/// 供抓包固化 client_secret 校验行为与 refresh_token 轮换语义。
/// 仅影响 OAuth 交换链路，签到/续期等其余直连请求不受影响。
fn exchange_agent() -> Result<ureq::Agent, String> {
    let addr = std::env::var("AIWORK_OAUTH_DEBUG_PROXY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(addr) = addr else {
        return Ok(short_agent());
    };
    // 调试代理容错（2026-09-16 实测）：设置了调试代理但尚未启动过代理（CA 未生成）
    // 时降级直连并记日志，不阻断登录闭环；只有 CA 存在但损坏才视为错误
    let dir = std::env::var("APPDATA")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join(crate::state::DATA_DIR_NAME));
    let ca_path = dir.as_ref().map(|d| d.join("certs").join("ca.crt"));
    let ca_pem = match ca_path.as_deref().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(p) => p,
        None => {
            if let Some(d) = dir.as_ref() {
                fs_utils::app_log(
                    d,
                    "[OAuth] AIWORK_OAUTH_DEBUG_PROXY 已设置但本地 CA 缺失/读取失败，交换请求降级直连；如需抓包请先启动一次代理生成证书",
                );
            }
            return Ok(short_agent());
        }
    };
    let der = pem_cert_der(&ca_pem)?;
    let mut roots = ureq::rustls::RootCertStore::empty();
    roots
        .add(ureq::rustls::pki_types::CertificateDer::from(der))
        .map_err(|e| format!("本地 CA 载入失败: {e}"))?;
    let config = ureq::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(4)
        .proxy(
            ureq::Proxy::new(&addr)
                .map_err(|e| format!("AIWORK_OAUTH_DEBUG_PROXY 值无效（{addr}）: {e}"))?,
        )
        .tls_config(std::sync::Arc::new(config))
        .build())
}

/// PEM(CERTIFICATE) → DER（与 device_proxy/ca.rs::pem_to_der 同实现，避免跨模块 pub 暴露）
fn pem_cert_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let body = pem
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|s| s.split("-----END CERTIFICATE-----").next())
        .ok_or_else(|| "本地 CA 文件缺少 CERTIFICATE PEM 块".to_string())?;
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("本地 CA base64 解码失败: {e}"))
}

/// 生成随机 hex 字符串。
/// 熵源：OS CSPRNG（Windows BCryptGenRandom 系统首选 RNG）。旧 LCG 以时间戳作种子，
/// 输出可预测，不适合 OAuth state / machine_id 等安全场景（审查 P2）；BCrypt 失败时
/// 保留 LCG 兜底（仅影响随机性，不中断流程）。
pub(crate) fn random_hex(len: usize) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = vec![0u8; len.div_ceil(2)];
        let halg: windows_sys::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE =
            unsafe { std::mem::zeroed() };
        // STATUS_SUCCESS == 0
        let status = unsafe {
            BCryptGenRandom(halg, bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status == 0 {
            let mut out = String::with_capacity(len);
            for b in bytes {
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
            }
            return out;
        }
    }
    // 兜底：旧 LCG（仅非 Windows 或 BCrypt 调用失败时）
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        // 简单 LCG
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let nibble = ((seed >> 32) & 0xF) as u8;
        out.push(if nibble < 10 {
            (b'0' + nibble) as char
        } else {
            (b'a' + nibble - 10) as char
        });
    }
    out
}

/// OAuth 登录设备标识（F-78 批次 3）：持久化于 data/oauth_device.json。
/// 原实现每次随机生成 machine_id/device_id，与 device_map.json 的账号稳定伪设备漂移，
/// OAuth 换发的 JWT 绑定设备与签到用设备不一致，存在被服务端判定异动/顶替的风控隐患。
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct OAuthDevice {
    pub machine_id: String,
    pub device_id: String,
}

/// 读取（缺失则生成并写回）本机稳定的 OAuth 设备标识。
/// device_id 优先对齐 device_map.json 已有条目（登录前无法预知账号，取 user_id 字典序
/// 最小的条目作为本机基准，保证不再每次随机）；machine_id 无对应字段，首次随机后固定。
/// 注意：不回写 device_map.json——DeviceEntry 三元组（device_id/market_user_id/session_id）
/// 与 OAuth 二元组字段语义不同，部分写入会破坏签到脚本的完整三元组假设。
fn load_or_create_oauth_device(state: &AppState) -> OAuthDevice {
    // SQLite 化（P2）：oauth_device.json → kv `oauth_device`
    let store = crate::store::db(&state.data_dir);
    let mut dev: OAuthDevice = store.kv_get("oauth_device");
    if dev.machine_id.is_empty() || dev.device_id.is_empty() {
        if dev.device_id.is_empty() {
            let map: DeviceMap = crate::store::docs::device_map_load(&crate::store::db(&state.data_dir));
            if let Some((_, entry)) = map.iter().min_by_key(|(k, _)| k.as_str()) {
                if !entry.device_id.is_empty() {
                    dev.device_id = entry.device_id.clone();
                }
            }
        }
        if dev.machine_id.is_empty() {
            dev.machine_id = random_hex(32);
        }
        if dev.device_id.is_empty() {
            dev.device_id = (0..15)
                .map(|_| {
                    let n = (random_hex(2).chars().next().unwrap() as u8).wrapping_rem(10);
                    (b'0' + n) as char
                })
                .collect();
        }
        // 写回失败不阻断登录（下次重新生成，仅损失一次稳定性）
        let _ = store.kv_set("oauth_device", &dev);
    }
    dev
}

/// 生成 PKCE code_verifier（RFC 7636：43-128 字符非 reserved 字符；hex 64字符合规）
/// 与 S256 code_challenge（BASE64URL-NOPAD(SHA256(verifier))）
fn pkce_pair() -> (String, String) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let verifier = random_hex(64);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// 生成 OAuth 登录 URL（2026-09-16 抓包固化：对齐真实 Trae IDE 登录页参数形态）。
/// 旧形态（client_secret/app_id/response_type=code）会让授权页停在
/// cn_credits_billing_status 后无后续、不回跳；真实流程：
/// login_channel=native_ide → 页面前端调 GetPCAuthCode（绑定 PKCE challenge）→
/// 302 回 auth_callback_url，回调参数为 authCodeInfo（JSON）而非 refreshToken/code。
#[tauri::command]
pub fn oauth_get_login_url(state: State<AppState>) -> OAuthLoginUrl {
    let state = &*state;
    let dev = load_or_create_oauth_device(state);
    let machine_id = dev.machine_id;
    let device_id = dev.device_id;
    // login_trace_id 兼作 CSRF 绑定值（抓包实证：授权页原样回传为回调 loginTraceID）
    let trace_id = random_hex(32);
    let (pkce_verifier, code_challenge) = pkce_pair();

    let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows-PC".into());
    let url = format!(
        "https://www.trae.cn/authorization?\
        login_version=1\
        &auth_from=trae\
        &login_channel=native_ide\
        &plugin_version={plugin_version}\
        &auth_type=local\
        &client_id={client_id}\
        &redirect=0\
        &login_trace_id={trace_id}\
        &auth_callback_url={redirect_uri}\
        &machine_id={machine_id}\
        &device_id={device_id}\
        &x_device_id={device_id}\
        &x_machine_id={machine_id}\
        &x_device_brand={hostname}\
        &x_device_type=windows\
        &x_os_version={os_version}\
        &x_env=\
        &x_app_version={app_version}\
        &x_app_type=stable\
        &code_challenge={code_challenge}\
        &code_challenge_method=S256\
        &channel_name=common",
        plugin_version = OAUTH_PAGE_PLUGIN_VERSION,
        client_id = oauth_client().client_id,
        trace_id = trace_id,
        redirect_uri = urlencoding::encode(OAUTH_REDIRECT_URI),
        machine_id = machine_id,
        device_id = device_id,
        hostname = urlencoding::encode(&hostname),
        os_version = urlencoding::encode("Windows"),
        app_version = OAUTH_PAGE_APP_VERSION,
        code_challenge = code_challenge,
    );

    // 记录本机登录会话（CSRF + PKCE）供回调校验/交换使用
    {
        let mut guard = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(PendingLogin { state: trace_id.clone(), pkce_verifier });
    }

    OAuthLoginUrl {
        url,
        state: trace_id,
        redirect_uri: OAUTH_REDIRECT_URI.to_string(),
    }
}

/// 解析 OAuth 回调 URL
#[tauri::command]
pub fn oauth_parse_callback(
    state: State<AppState>,
    callback_url: String,
) -> Result<OAuthCallbackInfo, String> {
    // 回调 URL 格式：http://127.0.0.1:port/authorize?refreshToken=xxx&accessToken=xxx&userId=xxx&userName=xxx&avatar=xxx
    // 或可能带 code 参数需要交换
    let query_str = callback_url
        .split('?')
        .nth(1)
        .ok_or_else(|| "回调 URL 中缺少查询参数".to_string())?;

    let mut params: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in query_str.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("").to_string();
        let value = kv.next().unwrap_or("").to_string();
        // URL decode
        let decoded = urlencoding::decode(&value)
            .map(|c| c.to_string())
            .unwrap_or(value);
        params.insert(key, decoded);
    }

    let user_id = params
        .get("userId")
        .or_else(|| params.get("user_id"))
        .or_else(|| params.get("UserID"))
        .cloned();

    let user_name = params
        .get("userName")
        .or_else(|| params.get("user_name"))
        .or_else(|| params.get("nickname"))
        .cloned();

    let avatar = params.get("avatar").cloned();

    // 抓包固化（2026-09-16）主路径：授权页 native_ide 流程 302 回调携带
    // authCodeInfo=<URL编码JSON>{"AuthCode","ExpireAt","ExpireDuration"} 与
    // userInfo=<JSON>{"UserID","ScreenName","AvatarUrl",...}、loginTraceID、host、
    // userRegion——从这两个 JSON 里补全身份信息（免调 GetUserInfo）
    let mut auth_code: Option<String> = None;
    if let Some(raw) = params.get("authCodeInfo") {
        let v: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| format!("authCodeInfo 解析失败（{e}）：授权页回调格式异常"))?;
        let code = v
            .get("AuthCode")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("authCodeInfo 中缺少 AuthCode 字段")?
            .to_string();
        auth_code = Some(code);
    }
    if let Some(raw) = params.get("userInfo") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            if user_id.is_none() {
                if let Some(uid) = v.get("UserID").and_then(|x| x.as_str()) {
                    params.insert("UserID".into(), uid.to_string());
                }
            }
            if let Some(name) = v
                .get("ScreenName")
                .or_else(|| v.get("NickName"))
                .and_then(|x| x.as_str())
            {
                params.insert("userName".into(), name.to_string());
            }
            if let Some(ava) = v.get("AvatarUrl").and_then(|x| x.as_str()) {
                params.insert("avatar".into(), ava.to_string());
            }
        }
    }
    // 上面 params 插入后重新取值（保持下游逻辑单一出口）
    let user_id = params.get("UserID").cloned().or(user_id);
    let user_name = params.get("userName").cloned().or(user_name);
    let avatar = params.get("avatar").cloned().or(avatar);

    // CSRF 校验（抓包固化：state 已不适用，native_ide 流程回调不回传 state，改用
    // loginTraceID 双向绑定——授权页把 login_trace_id 原样回传为 loginTraceID）。
    // 本进程签发过登录会话且回调携带 loginTraceID 时两者必须一致；回调不带
    // loginTraceID 或本进程未签发过（重启后粘贴回调）时保持宽容，不阻断正常登录。
    if let Some(cb_trace) = params.get("loginTraceID").or_else(|| params.get("login_trace_id")) {
        let issued = LAST_OAUTH_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.state.clone());
        if let Some(expected) = issued {
            if !expected.is_empty() && cb_trace != &expected {
                return Err("OAuth loginTraceID 校验失败：回调 URL 与本机发起的登录请求不匹配（可能为伪造或重放），已拒绝".into());
            }
        }
    } else if auth_code.is_some() {
        // 新流程回调必带 loginTraceID：缺失且本机有在途会话时视为不匹配
        let issued = LAST_OAUTH_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.state.clone());
        if issued.is_some() {
            return Err("OAuth 回调缺少 loginTraceID，无法确认与本机登录请求的对应关系，已拒绝".into());
        }
    }

    let mut access_token = params
        .get("accessToken")
        .or_else(|| params.get("access_token"))
        .cloned();

    // 主路径（抓包固化）：authCodeInfo.AuthCode → ExchangeToken（带 PKCE verifier）。
    // 兼容路径：refreshToken 直传（老形态）、code 参数（标准授权码）
    let refresh_token = match params
        .get("refreshToken")
        .or_else(|| params.get("refresh_token"))
        .cloned()
    {
        Some(t) => t,
        None => {
            let code = auth_code
                .or_else(|| params.get("code").cloned())
                .ok_or_else(|| "回调 URL 中缺少 authCodeInfo/refreshToken 参数".to_string())?;
            let verifier = LAST_OAUTH_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|p| p.pkce_verifier.clone());
            let device_id = load_or_create_oauth_device(&state).device_id;
            match exchange_code(&code, verifier.as_deref(), &device_id, &state.data_dir) {
                Ok((at, rt)) => {
                    access_token = Some(at);
                    rt
                }
                Err(e) => {
                    return Err(format!(
                        "AuthCode 交换失败（{e}）；可复制完整回调 URL 与 app.log 中的交换诊断反馈排查，或改用手动登录兜底"
                    ));
                }
            }
        }
    };

    Ok(OAuthCallbackInfo {
        refresh_token,
        access_token,
        user_id,
        user_name,
        avatar,
    })
}

/// 用 AuthCode 交换 token（抓包固化 2026-09-16：授权页 GetPCAuthCode 签发的
/// AuthCode 绑定 PKCE challenge，交换请求须带对应 CodeVerifier）。
/// 交换端点/响应结构未抓到（IDE 原生进程发起，浏览器 DevTools 抓不到）：
/// 请求体对齐 GetPCAuthCode 的 PascalCase 形态（ClientID/Code/CodeVerifier/
/// DeviceID/PlatformCode）；响应同时含 access_token 与 refresh_token 才算成功，
/// 失败时输出响应键路径（脱敏，不含值）供下一步校准。
fn exchange_code(code: &str, verifier: Option<&str>, device_id: &str, data_dir: &std::path::Path) -> Result<(String, String), String> {
    let resp = exchange_agent()?
        .post(&oauth_client().exchange_url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({
            "ClientID": oauth_client().client_id,
            "Code": code,
            "CodeVerifier": verifier.unwrap_or(""),
            "ClientSecret": oauth_client().client_secret,
            "DeviceID": device_id,
            "PlatformCode": OAUTH_PAGE_PLATFORM_CODE,
            "UserID": ""
        }))
        .map_err(|e| format!("ExchangeToken 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let code_val = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code_val != 0 {
        // 诊断（脱敏）：输出响应键路径，便于校准交换端点的真实响应结构
        let mut paths = Vec::new();
        collect_key_paths_public(&body, &mut paths);
        fs_utils::app_log(
            data_dir,
            &format!(
                "OAuth AuthCode 交换失败 (code={code_val}): 响应键路径: {}",
                paths.join(" | ")
            ),
        );
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        return Err(format!("ExchangeToken 失败 (code={}): {}", code_val, msg));
    }

    let data = body.get("data").ok_or("响应中缺少 data 字段")?;
    let access_token = data
        .get("access_token")
        .or_else(|| data.get("AccessToken"))
        .or_else(|| data.get("token"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?
        .to_string();
    let refresh_token = data
        .get("refresh_token")
        .or_else(|| data.get("RefreshToken"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 refresh_token")?
        .to_string();

    Ok((access_token, refresh_token))
}

/// 键路径收集（oauth.rs 本地版，脱敏：仅键名不含值）
fn collect_key_paths_public(v: &serde_json::Value, out: &mut Vec<String>) {
    fn rec(v: &serde_json::Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 6 || out.len() >= 40 {
            return;
        }
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    out.push(p.clone());
                    rec(val, &p, depth + 1, out);
                }
            }
            serde_json::Value::Array(a) => {
                for (i, x) in a.iter().enumerate().take(3) {
                    rec(x, &format!("{prefix}[{i}]"), depth + 1, out);
                }
            }
            _ => {}
        }
    }
    rec(v, "", 0, out)
}

/// ExchangeToken：用 refresh_token 换取 access_token
fn exchange_token(refresh_token: &str) -> Result<(String, Option<String>), String> {
    let resp = exchange_agent()?
        .post(&oauth_client().exchange_url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({
            "ClientID": oauth_client().client_id,
            "RefreshToken": refresh_token,
            "ClientSecret": oauth_client().client_secret,
            "UserID": ""
        }))
        .map_err(|e| format!("ExchangeToken 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        return Err(format!("ExchangeToken 失败 (code={}): {}", code, msg));
    }

    let data = body.get("data").ok_or("响应中缺少 data 字段")?;

    let access_token = data
        .get("access_token")
        .or_else(|| data.get("token"))
        .and_then(|v| v.as_str())
        .ok_or("响应中缺少 access_token")?;

    let new_refresh_token = data
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok((access_token.to_string(), new_refresh_token))
}

/// GetUserInfo：获取用户信息
fn get_user_info(access_token: &str) -> Result<(String, String), String> {
    let auth = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    let resp = exchange_agent()?
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({}))
        .map_err(|e| format!("GetUserInfo 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let data = body.get("data").or(body.get("result")).ok_or("响应中缺少 data 字段")?;

    let user_id = data
        .get("user_id")
        .or_else(|| data.get("UserID"))
        .or_else(|| data.get("userId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let user_name = data
        .get("name")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("userName"))
        .or_else(|| data.get("nickname"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((user_id, user_name))
}

/// OAuth 登录闭环：解析回调 → 换取 accessToken → 获取用户信息 → 保存账号
// async：内含最多两次 120s 超时的串行网络请求（exchange_token/get_user_info），同步命令会冻结 UI（审查修复）
#[tauri::command(async)]
pub fn oauth_login(
    state: State<AppState>,
    runtime: State<'_, std::sync::Mutex<Option<crate::commands::api_server::ApiServerRuntime>>>,
    callback_url: String,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    // 1. 解析回调 URL
    let callback_info = oauth_parse_callback(state.clone(), callback_url)?;

    // 2. 如果回调中没有 accessToken，则用 refresh_token 换取
    let (access_token, new_refresh_token) = if let Some(ref at) = callback_info.access_token {
        (at.clone(), None)
    } else {
        exchange_token(&callback_info.refresh_token)?
    };

    // 3. 规范化 JWT 格式
    let jwt = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.clone()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    // 4. 解析 JWT 获取 user_id
    let jwt_info = jwt::parse(&jwt);
    let user_id = callback_info
        .user_id
        .clone()
        .or_else(|| jwt_info.user_id.clone())
        .ok_or_else(|| "无法从回调或 JWT 中获取 user_id".to_string())?;

    // 5. 尝试获取用户名
    let name = account_name
        .or(callback_info.user_name.clone())
        .or_else(|| {
            // 尝试调用 GetUserInfo
            get_user_info(&jwt)
                .map(|(uid, uname)| if uname.is_empty() { uid } else { uname })
                .ok()
        })
        .unwrap_or_else(|| {
            // 按字符截取（字节切片在多字节 UTF-8 边界处会 panic）
            let head: String = user_id.chars().take(8).collect();
            format!("账号_{head}")
        });

    // 6. 确定最终的 refresh_token（优先使用 ExchangeToken 返回的新 token）
    let final_refresh_token = new_refresh_token
        .unwrap_or_else(|| callback_info.refresh_token.clone());

    // 7. 检查账号是否已存在
    let mut accounts = crate::vault::load_accounts(&state);
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&user_id))
    {
        // 已存在：更新 JWT 和 refresh_token
        let acct = accounts
            .accounts
            .iter_mut()
            .find(|a| a.user_id.as_deref() == Some(&user_id))
            .unwrap();
        acct.jwt = jwt.clone();
        acct.refresh_token = Some(final_refresh_token.clone());
        acct.updated_at = Some(fs_utils::now_iso());
        // 重新 OAuth 登录拿到新 token：生命周期计数清零、失效标记解除（F-78 批次 3）
        acct.refresh_token_fails = 0;
        acct.refresh_token_invalid = false;
        crate::vault::save_accounts(&state, &mut accounts)?;

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：更新已有账号 [{}] jwt + refresh_token", name),
        );
    } else {
        // 新账号
        accounts.accounts.push(RawAccount {
            name: name.clone(),
            user_id: Some(user_id.clone()),
            jwt: jwt.clone(),
            refresh_token: Some(final_refresh_token.clone()),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
            dc_id: None,
            refresh_token_expires_at: None,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            // 凭证最近落盘时间（F-78 批次 3 收尾，对齐 Buddy auth_saved_at）
            auth_saved_at: Some(fs_utils::now_iso()),
        });
        crate::vault::save_accounts(&state, &mut accounts)?;

        // 设置分组
        if let Some(g) = group_id {
            let mut groups: crate::models::GroupsFile =
                crate::store::docs::groups_load(&crate::store::db(&state.data_dir));
            groups.membership.insert(user_id.clone(), g);
            crate::store::docs::groups_save(&crate::store::db(&state.data_dir), &groups)?;
        }

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：新增账号 [{}] user_id={}", name, user_id),
        );
    }

    // F-78 批次 3：重新登录拿到新凭证 → 运行中 API 池回填 JWT 并解除 refresh_token 失效禁用
    {
        let guard = runtime.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rt) = guard.as_ref() {
            rt.shared.pool.note_refresh_success(&user_id, &jwt);
            rt.shared.wb_pool.note_refresh_success(&user_id, &jwt);
        }
    }

    Ok(OAuthLoginResult {
        user_id: user_id.clone(),
        name,
        jwt,
        refresh_token: final_refresh_token,
        has_refresh_token: true,
    })
}
