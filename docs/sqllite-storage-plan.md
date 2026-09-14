# 数据存储层 SQLite 化：data 目录 JSON 全量迁移落地方案

> **文档版本**: v2.0 · 2026-09-15
> **定位**: 数据存储层 SQLite 化（data 目录 JSON 全量迁移）的唯一权威执行文档。取代 `docs/tmp/sqlite-migration.md`（v1 会话稿）；`docs/rust-migration-and-storage-plan.md` §二「选择性迁移」立场已被全量迁移决策取代。
> **来源**: v1 计划 + 2026-09-15 全仓代码实测盘点（92 个 .rs、~250 处 JSON 读写调用面、44 个文件），逐文件核对读写点/struct/收敛入口。
> **前提确认**: Python 伴生脚本与 PowerShell 桥**已全部 Rust 化并移除**（`src-python/`、`src-ps/`、`prepare_python_runtime.py` 均不存在，无任何 python 子进程调用）→ **v1 计划的 Phase 3（Python/PS 跨进程改造）整体取消**，跨进程并发仅剩 Rust 进程内多线程 + `--task-run` CLI 子进程（同一 exe，共享同一 store 模块）。

---

## 一、现状重分析（相对 v1 计划的差异）

### 1.1 不变的事实

- 数据根目录 `%APPDATA%\AIWorkAssistant`：`conf/`（app_settings.json）、`data/`、`logs/`；`state.path(name)` 兼容路由（`app_settings.json → conf/`，其余 → `data/`）。
- 读写统一走 `fs_utils::read_json`（缺省 Default）/ `write_json`（tmp+rename 原子写）/ `read_json_cached`（mtime+size 解析缓存，仅 6 个网关热路径文件使用）。
- `rusqlite = { version = "0.32", features = ["bundled"] }` 已在依赖（Cargo.toml:36）。
- 读-改-写竞态（丢失更新）依旧是唯一真实数据丢失风险点；`credits_history`/`usage_history` 持续追加；无 schema 版本机制。

### 1.2 与 v1 计划的差异（本次实测修正）

| # | 差异 | 说明 |
|---|---|---|
| 1 | **Python/PS 已移除** | v1 Phase 3 取消；无需 `appstore.py`；PS 桥解耦已完成（f172726）。跨进程写方只剩 CLI 任务子进程（同 exe 同代码） |
| 2 | **wb_upstream 死引用现状** | `api_server/wb_upstream.rs:338/431` 读写 `data_dir` **根**的 `workbuddy_token_store.json`（正牌在 `data/`），不只是死读——431 行还会在根目录**写**文件。迁移时一并修正 |
| 3 | **新增盘点出的文件**（v1 未列全） | `groups.json`、`oauth_device.json`、`scheduler_state.json`、`doubao_captured_credentials.json`、`workbuddy_usage_official_all_cache.json`、`oauth_bypass_pending.json`、`last_proxy_port.txt`、`chat_backup_meta.json` |
| 4 | **调用面更分散** | 多写点文件实测：app_settings（3 写点）、groups（accounts.rs 7 写点）、account_cooldowns（tasks/commands/device_proxy 三域）、device_map、doubao_accounts（commands 与 tasks 双套直读写）、wb_accounts（tasks 绕过 common::load/save） |
| 5 | **热路径实为 6 文件** | dispatch_policy / wb_model_route / wb_template_map / wb_model_catalog / api_models / custom_models（read_json_cached 全部消费点） |
| 6 | **建模策略修订** | 见 §二：纯流水/指标表保持列化；实体池表改「行文档表 (pk, data JSON)」，避免 40+ 列机械映射引入回归（理由见 2.3） |

### 1.3 收敛度分级（决定切换工作量）

- ✅ **收敛良好**（改 load/save 即全量生效，~28 文件）：api_keys、api_usage、gateway_settings、dispatch_policy、api_models、custom_models、wb_catalog、trae_model_meta、wb_model_route、wb_template_map、wb_sticky、checkin_results、usage_history、scheduler_state、pay_status、oauth_device、api_pool、workbuddy_settings、workbuddy_credits_cache/history、3 个 official/activity 缓存、wb_cli_rotate_state、token_stats_files、wb_common 三件套（token store/checkin results/pool 路径）、doubao_renew_result、checkin_summary。
- ⚠️ **多写点分散**（切换时逐点替换）：app_settings、groups、account_cooldowns、device_map、remaining_credits、doubao_health_history、checkin_accounts（device_proxy 捕获旁路）。
- ❌ **双套直读写**：doubao_accounts（`commands::doubao::load_pool/save_pool` vs `tasks/doubao_*` 直拼路径）——切换时统一收敛到 load_pool/save_pool。

---

## 二、存储设计

### 2.1 基础设施

- 库文件：`<data_dir>/data/aiwork.sqlite`，WAL 模式（`-wal/-shm` 常驻属正常）。
- 连接：`Store { conn: Mutex<Connection> }`，进程级注册表按 `data_dir` 缓存（`store::db(data_dir) -> Arc<Store>`）；`PRAGMA journal_mode=WAL; synchronous=NORMAL; busy_timeout=5000; foreign_keys=ON`。
- schema 版本：`PRAGMA user_version`（v1 = 本迁移）。
- 单连接串行：个人应用 QPS 低；热路径 6 文件的 mtime 解析缓存由「单行/单键 SELECT+serde 解析（µs 级）」替代，`read_json_cached` 与 json_cache 整体删除。

### 2.2 表模型（三组）

#### ① KV 文档表（23 键）——content 保持原 serde JSON 结构，key = 文件名去 .json

```sql
CREATE TABLE kv (key TEXT PRIMARY KEY, content TEXT NOT NULL,
  updated_at TEXT NOT NULL DEFAULT (datetime('now','localtime')));
```

成员：`app_settings`、`api_pool`、`dispatch_policy`、`api_gateway_settings`、`api_models`、`wb_model_catalog`、`trae_model_meta`、`wb_model_route`、`wb_template_map`、`wb_sticky_sessions`、`checkin_summary`、`workbuddy_settings`、`workbuddy_credits_history`、`workbuddy_credits_cache`、`workbuddy_usage_official_cache`、`workbuddy_usage_official_all_cache`、`workbuddy_activity_cache`、`wb_cli_rotate_state`、`token_stats_files`、`doubao_renew_result`、`usage_history`、`oauth_device`、`scheduler_state`。
（整存整取、无行级访问诉求 → KV 最贴切；`remaining_credits`/`api_keys`/`custom_models` 的顶层 `updated_at` 全局字段随行文档表入 kv 键 `<name>_updated_at`。）

#### ② 行文档实体表（12 表）——(pk TEXT PRIMARY KEY, data TEXT NOT NULL JSON, updated_at)

| 表 | 源文件 | pk | data 内容（serde JSON，struct 不变） |
|---|---|---|---|
| `accounts` | checkin_accounts.json | 行号（INTEGER PK，user_id 可空） | `Account` 全字段 |
| `device_map` | device_map.json | uid | `DeviceEntry` |
| `groups` | groups.json | uid | group_id（TEXT） |
| `remaining_credits` | remaining_credits.json | uid | 每账号积分聚合对象（credits/expire_time/general/work/total_limit/membership_*） |
| `account_cooldowns` | account_cooldowns.json | uid | `CooldownEntry` |
| `pay_status` | pay_status.json | uid | `PayStatusEntry` |
| `api_keys` | api_keys.json | key id | `ApiKeyEntry`（allowed_accounts 为 JSON 数组文本） |
| `custom_models` | custom_models.json | model id | `CustomModel` |
| `doubao_accounts` | doubao_accounts.json | user_id | `DoubaoAccount` |
| `wb_accounts` | workbuddy_accounts.json | account id | `WorkBuddyAccount` |
| `wb_tokens` | workbuddy_token_store.json | account_id | token 记录（accessToken/refreshToken/expires/domain） |
| `doubao_captured_credentials` | doubao_captured_credentials.json | uid | 凭据对象（MITM 捕获写 / 命令读） |

**建模修订理由（相对 v1「全字段列化」）**：实体表消费模式全部是「整文件读入内存 struct ↔ 整文件写回」，无 SQL 级字段过滤/聚合诉求；列化需为 12 个 struct 各写 30~40 列机械映射（≈1500 行纯胶水），回归风险远大于收益。行文档表保留行级 PK（UPSERT/行删/演进能力）与事务原子性，serde 结构零改动、调用面零改动。纯流水/指标表（无固定 struct、需日期聚合与裁剪）保持列化（③）。

#### ③ 列化流水/指标表（7 表）

| 表 | 源文件 | 列 |
|---|---|---|
| `credits_history` | credits_history.json | id INTEGER PK AUTOINCREMENT; date/user_id/credits(+delta，以 struct 为准) |
| `credits_daily` | credits_daily.json | date TEXT PK; total/earned/consumed REAL |
| `checkin_results` | checkin_results.json | PK(day, uid); name/status/updated_at |
| `wb_checkin_results` | workbuddy_checkin_results.json | id INTEGER PK; day/uid/name/status/message/time/reward REAL |
| `api_usage` | api_usage.json | PK(bucket, day, dim); requests/ok/errors/prompt_tokens/completion_tokens/duration_ms INTEGER |
| `api_key_daily` | api_keys.json daily_stats | PK(key_id, date); requests/prompt_tokens/completion_tokens 等 |
| `doubao_health_events` | doubao_health_history.json | id INTEGER PK; day TEXT; payload TEXT(JSON)；裁剪改 DELETE 超限旧行 |

（列名以对应 struct 的 serde 字段 snake_case 为准，实现时核对 models.rs / 模块内定义；serde default → 列 DEFAULT。）

### 2.3 排除项（不迁移）

`conf/vault.stronghold`、`conf/vault_key.bin`、`data/profiles*/`、`data/certs/`、`logs/`、`data/workbuddy_chats/`、`data/exports/`（含 `chat_backup_meta.json`）、`task_*.cmd`、`last_proxy_port.txt`、`oauth_bypass_pending.json`（崩溃残留标记，生命周期极短，保留文件）、`oauth_client.json`（用户手工外置配置，仅读，保留文件）、外部应用文件（~/.codebuddy、Trae state.vscdb、豆包 public_config 等）。

### 2.4 遗留根路径兜底迁移（存在才处理）

`workbuddy_token_store.json`（wb_upstream 死引用产物）、`wb_template_map.json`、`api_models.json`、`wb_sticky_sessions.json`、`wb_model_route.json`、`checkin_accounts.json`（如出现在根）。导入后移 `backup/legacy_root/`，并删除代码中的旧根路径只读兼容分支。

---

## 三、迁移器设计（`store::migrate::migrate_on_startup(data_dir) -> Option<String>`）

- 库已存在且 `user_version >= 1` → 跳过（幂等）。
- 否则：建表 → 按 FILE_REGISTRY 逐文件三态处理：
  1. **正常**：解析 JSON（serde 现有类型）→ 写目标表/键 → `fs::rename` 原文件到 `data/backup/<相对路径>`（保留 conf/data 前缀，同盘原子）；
  2. **损坏**：解析失败 → 原文件移 `data/backup/corrupt/` + 记日志（对齐现 read_json「坏文件回退默认值」行为）；
  3. **缺失**：跳过。
- 全部处理完才置 `user_version = 1`；写 `data/backup/migration_manifest.json`（每文件结果 + 时间戳）。
- 失败不阻断启动（摘要返回调用方写 app_log）。
- main.rs setup：`vault::migrate_on_startup` 之后调用（vault 先抽离敏感字段，再迁库）。

## 四、实施阶段（每阶段：开发 → 全面审查修复 → cargo test + tsc → git 提交）

| 阶段 | 内容 | 验收 |
|---|---|---|
| **P0** | 本计划文档 | 文档评审 |
| **P1** | store 基础设施：`store/{mod,schema,kv,migrate}.rs` + main.rs 接线 + 单测（kv roundtrip / 迁移三态 / 幂等重跑 / backup+manifest） | cargo test 全绿 |
| **P2** | KV 组切换（§2.2① 全部 23 键）：state.settings、misc/env 设置写、gateway_settings、dispatch、models_sync、wb_catalog、wb_model_route、wb_payload、unified_catalog、commands/api_server(api_pool)、workbuddy settings、credits 三缓存、cli rotate、token_stats、wb_sticky、doubao_renew_result、usage_history、oauth_device、scheduler_state、checkin_summary | cargo test 全绿；前端功能等价 |
| **P3** | 行文档/列化组切换：vault/accounts 域（accounts、groups、device_map、remaining_credits、account_cooldowns、credits_history、credits_daily、checkin_results、checkin_summary 读、pay_status）、trae_apps、oauth、api_keys(+api_key_daily)、usage(api_usage)、custom_models、workbuddy 域（wb_accounts、wb_tokens、wb_checkin_results、credits_history kv→保留）、doubao 域（doubao_accounts、health_events、captured_credentials，双套直读写收敛到 load_pool/save_pool）、scheduler_state | cargo test 全绿 |
| **P4** | 收尾：wb_upstream.rs 死引用修复（根路径 → `wb_tokens` 表）；热路径 6 文件改造确认（kv_get 直读）；`read_json_cached`/json_cache 删除；legacy 根路径兼容分支删除；`device_proxy` 捕获旁路（checkin_accounts/doubao_captured_credentials）切 store | cargo build 无 warning |
| **P5** | 全量验证：cargo test 全绿、cargo build 无 warning、前端 tsc 通过；迁移演练检查表（真实数据目录升级 / 空目录首启 / 幂等重跑 / backup 完整性 / manifest）；全部代码复审 | 检查表全过 |

## 五、红线（继承 v1 §2.5 并强化）

1. 迁移后**不允许新旧混读**：模块切换 = 该文件全部读写点同阶段内切换完毕。
2. 原 JSON 仅在启动迁移时刻移入 `backup/`；运行期不再产生任何迁移表中文件的 JSON 写入。
3. WAL + busy_timeout 必开；CLI 子进程（`--task-run`）与主进程并发访问同一库依赖 busy_timeout + 短事务。
4. 不改任何 tauri command 前端契约；响应结构不变。
5. `fs_utils::read_json/write_json` 保留（vault/日志/导出/backup manifest 仍用）；`read_json_cached` 随热路径切换整体删除。
6. 敏感字段（jwt/token）在库中为明文列/JSON——与现状一致，权威密文仍在 Stronghold，不扩大不缩小安全边界。

## 六、Verification（对照验收）

- `data/aiwork.sqlite` 生成；38+ 文件按注册表全部迁移、原 JSON 移入 `backup/`（含 manifest）✅ P1/P3
- 二次启动幂等不再迁移；空目录首启正常建空库 ✅ P1/P5
- 网关热路径（dispatch/目录聚合/每请求记账）行为与性能不回退 ✅ P4
- 全部单测绿 + build 无 warning + tsc 通过 ✅ 各阶段
