# Rust 去 Python 依赖 + 存储层 SQLite 化：分析与技术实现路径

> **文档版本**: v1.0 · 2026-09-13
> **定位**: 两项架构演进（① Python 脚本 Rust 重写去除 Python 依赖；② JSON 文本存储选择性 SQLite 化）的可行性分析、技术实现路径与分阶段交付计划。
> **来源**: 2026-09-13 基于 `src-python/`（9 个主脚本 ~6600 行）与 `data/conf/` 实测数据（34 个状态 JSON、47 处写 / 80 处读调用面）的盘点分析。
> **原则**: 分阶段交付、每阶段可独立回退；不搞一次性大爆炸重写；Windows 现有行为零回归（247 个 Rust 测试 + 前端 tsc 为回归网）。

---

## 一、Python → Rust 重写（去除 Python 依赖）

### 1.1 现状盘点

| 脚本 | 行数 | 职责 | 重写难度 |
|---|---|---|---|
| `device_proxy.py` | **2122** | 自研 MITM 代理：CA 生成/签发、per-host 动态叶证、TLS 拦截、签到接口头改写、凭据捕获、WebSocket(pbbp2+deflate) 帧记录、SSE 摘要、上游 VPN 透传 | **中高（唯一的硬骨头，详见 §1.4）** |
| `doubao_chats.py` | 1235 | 豆包对话导出（分页 API → markdown/json） | 中 |
| `auto_checkin.py` | 735 | Trae 多账号签到（重试轮 + NDJSON 事件流 + summary 落盘） | 低 |
| `workbuddy_checkin.py` | 506 | Buddy 签到 | 低 |
| `wb_common.py` | 478 | Buddy 公共（auth 文件路径、接口封装） | 低 |
| `doubao_renew.py` | 570 | DPAPI cookie 解密 + KeepAlive 续期 | 低 |
| `doubao_quota.py` | 474 | 会员配额查询（端点待 F-24-余 固化） | 低 |
| `workbuddy_credits.py` | 405 | Buddy 积分查询 | 低 |
| `workbuddy_ui_click.py` | 80 | UI 自动化 | 低 |

**运行时**：`scripts/prepare_python_runtime.py` 构建期准备嵌入式 Python → 打包为 resource `python/`（估 40~80MB）；`python.rs` 以子进程方式拉起，注入 `AIWORKDATA_DIR`，Job Object 跟随主进程退出。

### 1.2 可行性结论

**可行且有价值，分批推进；`device_proxy.py` 单独评估、可长期保留 Python。**

- **低难度部分（~3800 行：签到/积分/配额/续期/导出）**：纯网络 + JSON。`ureq`（`workbuddy/credits.rs` 已在用——WorkBuddy 积分接口即 Rust 直调，**Python/Rust 双轨并存已验证此模式**）、`serde_json`、`chrono` 全部现成；DPAPI 在 `vault.rs` 现成。重写为确定性工程移植。
- **收益**：
  1. 打包体积 −40~80MB（去嵌入式 Python 运行时与解压）；
  2. 查询类操作免子进程冷启动（几百 ms → 同进程直调）；
  3. NDJSON 事件流直通 Tauri `emit`（少一层 stdout 桥接，与切换桥 Rust 化同理）；
  4. 单语言栈、测试并入 `cargo test`（现 Python tests 与 Rust 测试双轨维护）；
  5. 为 F-75（macOS）铺路——少管一个跨平台运行时。
- **成本/风险**：
  1. 一次性重写 + 测试对齐（§1.3 分批计划）；
  2. 协议脚本「端点/风控一变即改」的 Python 迭代优势消失——本工程无热发版诉求，可接受；
  3. **TLS/HTTP2 指纹差异需实测**（requests vs ureq 的 ClientHello 不同；目前签到无强指纹风控迹象，Rust 版上线前跑一次真实对照）。

### 1.3 分批实施路径

| 批次 | 内容 | 预估 | 验收 |
|---|---|---|---|
| **R1** | 签到/积分/配额/续期（auto_checkin / workbuddy_checkin / workbuddy_credits / wb_common / doubao_quota / doubao_renew）：Rust 命令直调上游接口；签到重试轮改为 tokio 任务；NDJSON 事件 → `emit`；summary 落盘复用 `write_json` | ~1 周 | 双实现并行对照一轮真实签到（结果/事件序列一致）；Python 侧对应脚本下线 |
| **R2** | `doubao_chats.py` 对话导出 → Rust 命令（分页拉取 + markdown/json 双格式，复用现有导出目录约定） | 2~3 天 | 导出内容与 Python 版 diff 一致 |
| **R3** | `device_proxy.py` MITM 代理 → 独立模块 `src-tauri/src/device_proxy/`（§1.4） | 3~4 周（三阶段） | §1.4 验收标准 |
| **R4** | 收尾：`prepare_python_runtime.py` 与嵌入式运行时移除（若 R3 完成）或裁剪依赖瘦身（若 R3 缓行）；`workbuddy_ui_click.py` 评估去留 | 0.5~1 天 | 安装包体积/启动延迟实测 |

### 1.4 device_proxy.py Rust 重写专项（成功率 ~85~90%）

**为什么没有硬骨头**：该脚本本质是 socket + TLS + 字节流处理，无 Python 独有魔法（无反射/monkey-patch/C 扩展依赖），恰是 Rust 生态最完整的领域。逐项对照：

| 功能面 | Python 实现 | Rust 对应物 | 成熟度 |
|---|---|---|---|
| 代理骨架（thread-per-conn、128 并发信号量、`SO_EXCLUSIVEADDRUSE` 独占绑定、端口占用报 PID） | socket + threading | tokio + socket2 | 极成熟 |
| CONNECT 分流（目标域 MITM / 其余透明隧道保 VPN） | 手写字节流 | 同逻辑移植或 hyper | 极成熟 |
| TLS 拦截（自签 CA + per-host 动态叶证 + 解密） | `cryptography` x509 + `ssl.SSLContext` | **rcgen**（CA/叶证签发）+ **rustls**（server 侧装叶证、client 侧信任自签 CA） | 工业级；Trae 为现代 Electron（TLS1.2/1.3），rustls 完全兼容 |
| 签到接口头改写（`x-device-id` / `x-market-user-id` / `vscode-sessionid`，按 JWT `data.id` 映射） | 手写 HTTP 解析 + 头替换 | hyper header 注入（keep-alive/chunked 处理更稳） | 成熟 |
| JWT / refresh_token / 豆包凭据捕获写回 accounts | 正则 + json + DPAPI/sqlite | 同逻辑；DPAPI/rusqlite 项目现成 | 成熟 |
| WebSocket MITM（pbbp2 + permessage-deflate 帧解析重组，~250 行） | 手写帧解析 | **照 Python 版算法移植**（tungstenite deflate 支持不完整，不建议换库） | 中等，无未知领域 |
| SSE 摘要（metadata/output/token_usage） | 缓冲 + 行解析 | 相同 | 成熟 |
| gzip/zlib、100MB 滚动日志、脱敏 | gzip + 自写 | flate2、tracing-appender | 成熟 |
| 上游代理透传（VPN 兼容）、`--gen-ca` / `--capture-local` 子命令 | — | hyper 代理 connector / 子命令 | 成熟 |

**真实风险（不是"做不出来"，是"对不齐"）**：
1. **防御性细节必须逐条对照移植**——代理是全系统流量必经点，挂起/泄漏被放大。Python 版实战打磨过的坑：孤儿进程假启动（issue #7 独占绑定）、先握手后记日志防客户端 EOF、信号量过载拒绝、accept 异常退避等，漏一条即线上事故；
2. **TLS 指纹差异**：rustls ClientHello 与 Python ssl 不同，对上游（Trae 服务器）需一次真实流量实测；
3. 叶证签发性能：当前每次连接生成 RSA2048（Python 都扛得住，Rust 只会更快；可顺手加 LRU 缓存）。

**分阶段交付（把 85% 推到 ~99%）**：
- **阶段 A（1.5~2 周）**：CONNECT 分流 + MITM + 签到头改写 + 凭据捕获（已覆盖脚本 80% 核心价值）。验收：双版本分别接真实 Trae 客户端各跑一天，proxy.log 请求序列与捕获结果对照一致；
- **阶段 B（+1 周）**：WebSocket 帧记录 + SSE 摘要 + 滚动日志对齐；
- **阶段 C（+3~5 天）**：看门狗/独占绑定/上游代理/脱敏防御细节对齐 + 72h 稳定性烤机；
- **全程**：Python 版保留为回退开关，Rust 版设置页灰度启用，稳定一个版本周期后移除。

> **最坏形态**：阶段 B/C 延期，但阶段 A 完成即已可用——不存在走到一半发现死路、前功尽弃的项目形态。

---

## 二、JSON 文本存储 → SQLite（选择性迁移，不做全量换库）

### 2.1 现状事实

- `conf/` + `data/` 共 **34 个状态 JSON**；实测量级：最大 `token_stats_files.json` **320KB**（文件级增量缓存，条目数 ≈ 会话文件数），次大 `credits_history.json` 49KB，其余全部 < 12KB；
- `fs_utils::write_json` 已是 **tmp + rename 原子替换**（不撕裂）+ mtime/size 逐出缓存兜底热路径；**47 处写 / 80 处读**；
- **跨进程并发写真实存在**：Rust 命令层 + 定时任务线程 + Python（`device_proxy.py` 写 `device_map.json`/凭据缓存、`auto_checkin.py` 写 summary）。

### 2.2 现存问题（真实，但小）

1. **读-改-写竞态（丢失更新）**——原子 rename 只保证不撕裂，不保证不丢：两进程同时读 → 各自改 → 先后 rename，后写覆盖先写。真实场景：`device_map.json` 的 Python/Rust 双写方、多命令并发写同一池文件。**这是当前唯一可能造成实际数据丢失的点**；
2. `token_stats_files.json` 每次扫描**全量重写**，体积随会话文件数线性增长（当前 320KB 无感，十万会话级会到 MB）；
3. `credits_history` / `usage_history` 持续追加，前端全量拉取内存过滤（量级小暂无感）；
4. 无 schema 版本/迁移机制（serde default 兜底，可接受）。

### 2.3 SQLite 可行性与价值评估

- **可行性：高**——`rusqlite` 已是依赖（chatdata / workbuddy_stats 在用），零新增依赖；WAL 模式原生支持多进程读写；仍是单文件，备份/迁移语义不变。
- **价值：当前量级下偏低，全量换库是负收益**——绝大多数文件 < 5KB（配置/池/结果），换库付出迁移成本 + 丧失"记事本直读直改"可调试性 + Python 侧引入 sqlite3 改造，性能收益趋近于零。

### 2.4 实施路径：选择性换库 + 廉价止血

| 步骤 | 内容 | 时机 |
|---|---|---|
| **S1 止血丢失更新（不换库）** | 双写文件归口单写者：Python 侧（R1 后仅剩 device_proxy）只输出事件、由 Rust 落盘；或对少数双写文件加文件锁（`fs2`/`LockedFile`） | 随 R1/R3 |
| **S2 第一批换库** | `token_stats_files.json` → SQLite 表 `(path TEXT PK, mtime_ms, size, rev, agg_json)`：增量 UPSERT 替代全量重写，解析缓存逐出逻辑不变 | R3 完成后（设备捕获行为稳定时） |
| **S3 触发条件化** | `credits_history` / `usage_history` 超 ~1MB 或出现按日期区间查询诉求时，迁 SQLite（`date` 主键 + 区间查询替代全量拉取） | 按数据量触发 |
| **S4 多实例刚需** | F-67（TRAE 多实例并行）立项时，账号池类文件（`checkin_accounts.json` 等）统一入 SQLite——多进程并发写同一池那时才成为刚需 | 随 F-67 |
| **保持不变** | 其余配置类文件（settings / groups / dispatch_policy / 各池小文件）**保持 JSON**——原子写 + 缓存已够，直读可调试是优点 | — |

### 2.5 迁移红线

- 任何换库文件必须带一次性导入迁移（旧 JSON 存在 → 读入建表 → 改名 `.migrated.bak` 保留一代）；
- SQLite 文件纳入现有 vault 快照/恢复语义（与 JSON 同级对待）；
- WAL 模式开启 + `busy_timeout`，防多进程 `SQLITE_BUSY` 硬失败。

---

## 三、总排期与依赖关系

```
R1 签到/积分 Rust 化 ──→ R2 对话导出 ──→ R3 device_proxy 三阶段 ──→ R4 运行时移除
        │                      │                    │
        └── S1 双写归口 ───────┴── S2 缓存换库 ──────┴── S3/S4 按触发条件
```

- **总量级**：R1+R2 ≈ 1.5 周；R3 ≈ 3~4 周（含对照/烤机）；S 步骤按触发条件零星 1~3 天。
- **与 backlog 的关系**：R3 与 F-75（macOS，切换桥 Rust 化）共享 tokio/平台抽象底座（M0），建议底座先行一次投入两处复用；本文不改变 product-optimization-backlog.md 的优先级结论，立项时再行登记。
