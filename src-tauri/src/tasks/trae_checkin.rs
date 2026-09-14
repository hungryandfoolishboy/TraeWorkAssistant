//! Trae 多账号签到单轮执行（原 src-python/auto_checkin.py 的 Rust 移植，逐函数对齐）：
//! - 直连 api.trae.cn（ureq 不读系统/环境代理，对齐 python NO_PROXY=* 绕代理约定）
//! - status 预检（已签跳过）→ claim（仅网络异常按 retry 次数重试，业务失败不重试）→
//!   错误分类（classify_error）→ 冷却落盘（account_cooldowns.json）
//! - 积分归属三层兜底（claim 奖励字段 → 复查余额差值 → 旧行为，纯函数单测覆盖）
//! - NDJSON 事件契约（checkin-progress 管线）：start {type,total} /
//!   account {index,user_id,name,status,code,message,credits,delta,error_type,cooldown_until} /
//!   done {type,ok,already,failed}
//! - 侧写文件与 python 完全一致：device_map.json（新条目确定性派生后落盘共享）、
//!   account_cooldowns.json、credits_history.json（90 天滚动）、checkin_summary.json（同日合并）、
//!   logs/checkin.log（摘要行追加）
//! 红线：jwt 不进日志/事件（事件仅含 user_id/name/status 等脱敏字段）。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::TimeZone;
use serde_json::{json, Value};

use crate::commands::accounts::derive_device;
use crate::commands::oauth::random_hex;
use crate::fs_utils;
use crate::jwt;
use crate::models::{AccountCooldownsFile, CooldownEntry, DeviceEntry, RawAccount};
use crate::state::AppState;

use super::http_agent;

const SIGNIN_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/claim";
const STATUS_URL: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits/status";
/// JWT 剩余有效期低于该值（小时）时写入告警（对齐 python EXPIRY_WARN_HOURS）
const EXPIRY_WARN_HOURS: f64 = 24.0;
/// 网络异常重试间隔（对齐 python signin_with_retry 的 1s）
const RETRY_GAP: Duration = Duration::from_secs(1);

/// claim 响应中疑似「本次奖励」的候选字段（按优先级排列；data 层优先于顶层）。
/// 说明：status 顶层 credits 无法离线确证是「签到后余额」还是「可领奖励额度」，
/// 因此 delta 优先取 claim 响应自身的奖励字段（接口返回为准）。
const CLAIM_REWARD_KEYS: [&str; 10] = [
    "reward", "reward_credits", "claim_credits", "checkin_credits", "delta", "increase", "obtain",
    "gained", "credits", "amount",
];

/// 单轮结果（供命令层重试编排消费）：per-uid 最终状态与失败类型。
/// statuses: uid -> success/already/fail；error_types: 仅失败且分类非 Unknown 时登记
///（SessionDead=JWT 被吊销为永久失效，重试轮据此排除）。
#[derive(Default)]
pub struct RoundOutcome {
    pub statuses: HashMap<String, &'static str>,
    pub error_types: HashMap<String, String>,
}

// ── HTTP 层（对齐 python _build_headers / _http_post）──────────────────────

/// 签到/状态接口共用请求头（按账号独立设备 id 与 session）。
/// 不发送 accept-encoding：ureq 未启用 gzip 特性，避免收到无法解压的压缩响应。
fn build_headers(jwt: &str, dev: &DeviceEntry) -> Vec<(String, String)> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    vec![
        ("accept".into(), "*/*".into()),
        ("accept-language".into(), "zh-CN".into()),
        ("authorization".into(), auth),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), "VSCode 1.107.1 (TRAE SOLO CN)".into()),
        ("x-market-client-id".into(), "VSCode 1.107.1".into()),
        (
            "x-market-user-id".into(),
            dev.market_user_id.clone().unwrap_or_default(),
        ),
        ("x-user-region".into(), "CN".into()),
        ("x-device-id".into(), dev.device_id.clone()),
        ("x-lgw-req-sdk-type".into(), "3".into()),
        ("package-type".into(), "stable_cn".into()),
        ("x-request-id".into(), random_hex(32)),
        ("x-lscbd-aid".into(), "787976".into()),
        ("x-lscbd-platform".into(), "windows".into()),
        ("app-version".into(), "0.1.45".into()),
        ("x-tt-trace-id".into(), format!("00-{}-01", random_hex(16))),
        (
            "vscode-sessionid".into(),
            dev.session_id.clone().unwrap_or_default(),
        ),
        ("sec-fetch-dest".into(), "empty".into()),
        ("sec-fetch-mode".into(), "no-cors".into()),
        ("sec-fetch-site".into(), "none".into()),
    ]
}

/// 统一 POST 入口 → (http_status, parsed, raw_text)。
/// status 0=网络异常（对齐 python timeout→0 / exception→-1 归一为 0，原文进 raw 供排查）。
fn http_post(
    agent: &ureq::Agent,
    jwt: &str,
    dev: &DeviceEntry,
    url: &str,
) -> (i32, Option<Value>, String) {
    let mut req = agent.post(url);
    for (k, v) in build_headers(jwt, dev) {
        req = req.set(&k, &v);
    }
    match req.send_string("{}") {
        Ok(resp) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (200, parsed, raw)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let raw = resp.into_string().unwrap_or_default();
            let parsed = serde_json::from_str(&raw).ok();
            (code as i32, parsed, raw)
        }
        Err(e) => (0, None, e.to_string()),
    }
}

/// 设备标识解析（对齐 python get_device_for）：device_map.json 已有条目优先
/// （代理捕获/切换流程写入，与新 JWT 的设备指纹校验匹配），缺失时按 uid 确定性派生
///（与 python gen=2 同算法）并落盘共享给 device_proxy。
fn get_device_for(state: &AppState, uid: &str) -> DeviceEntry {
    let path = state.path("device_map.json");
    let map: Value = fs_utils::read_json(&path);
    if let Some(rec) = map.get(uid).filter(|v| v.is_object()) {
        if let Ok(e) = serde_json::from_value::<DeviceEntry>(rec.clone()) {
            if !e.device_id.is_empty() {
                return e;
            }
        }
    }
    let dev = derive_device(uid);
    if let Some(m) = map.as_object() {
        let mut updated = m.clone();
        updated.insert(uid.to_string(), serde_json::to_value(&dev).unwrap_or_default());
        let _ = fs_utils::write_json(&path, &Value::Object(updated));
    }
    dev
}

// ── status 预检 / claim（对齐 python status_check / _signin_request）────────

/// status 预检结果
struct StatusOutcome {
    ok: bool,
    checked_in: Option<bool>,
    credits: Option<i64>,
    message: String,
}

/// 预检今日是否已签。user_id 解析失败直接报错（与 python 一致，不发请求）。
fn status_check(
    state: &AppState,
    agent: &ureq::Agent,
    jwt: &str,
    user_id: Option<&str>,
) -> StatusOutcome {
    let none = |message: &str| StatusOutcome {
        ok: false,
        checked_in: None,
        credits: None,
        message: message.to_string(),
    };
    let Some(uid) = user_id else {
        return none("无法从 JWT 解析 user id");
    };
    let dev = get_device_for(state, uid);
    let (status, body, raw) = http_post(agent, jwt, &dev, STATUS_URL);
    if status == 0 {
        let msg = if raw.is_empty() { "网络异常".to_string() } else { raw };
        return none(&msg);
    }
    let Some(data) = body.filter(|b| b.is_object()) else {
        let head: String = raw.chars().take(200).collect();
        return StatusOutcome {
            ok: false,
            checked_in: None,
            credits: None,
            message: format!("非 JSON 响应: {head}"),
        };
    };
    let code = data.get("code").and_then(Value::as_i64);
    let checked_in = data.get("checked_in").map(py_truthy);
    let credits = int_candidate(data.get("credits").unwrap_or(&Value::Null));
    let msg = data.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    if code != Some(0) {
        return StatusOutcome {
            ok: false,
            checked_in,
            credits,
            message: if msg.is_empty() { format!("HTTP {status}") } else { msg },
        };
    }
    StatusOutcome { ok: true, checked_in, credits, message: msg }
}

/// 底层签到请求 + 响应解析 → (ok, msg, code, http_status, data)。
/// data 为解析出的 JSON（网络异常/非 JSON 时 None），供奖励字段提取；
/// code=None 同时是传输层异常标记（signin_with_retry 据此重试；
/// 非 JSON 响应不把 HTTP 状态码塞进业务 code，防 classify_error 误判）。
#[allow(clippy::type_complexity)]
fn signin_request(
    state: &AppState,
    agent: &ureq::Agent,
    jwt: &str,
    user_id: Option<&str>,
) -> (bool, String, Option<i64>, i32, Option<Value>) {
    let Some(uid) = user_id else {
        return (false, "无法从 JWT 解析 user id".into(), None, 0, None);
    };
    let dev = get_device_for(state, uid);
    let (status, body, raw) = http_post(agent, jwt, &dev, SIGNIN_URL);
    if status == 0 {
        let msg = if raw.is_empty() { "网络异常".to_string() } else { raw };
        return (false, msg, None, 0, None);
    }
    match body.filter(|b| b.is_object()) {
        Some(data) => {
            let code = data.get("code").and_then(Value::as_i64);
            let msg = data
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            (code == Some(0), msg, code, status, Some(data))
        }
        None => {
            let head: String = raw.chars().take(200).collect();
            (false, format!("HTTP {status}: 非 JSON 响应: {head}"), None, status, None)
        }
    }
}

/// 签到（含网络异常重试）：仅 code=None（传输层异常）按 retry 次数重试，业务失败不重试。
#[allow(clippy::type_complexity)]
fn signin_with_retry(
    state: &AppState,
    agent: &ureq::Agent,
    jwt: &str,
    user_id: Option<&str>,
    retry: u32,
) -> (bool, String, Option<i64>, i32, Option<Value>) {
    let mut last = (false, "无重试".to_string(), None, 0, None);
    for attempt in 0..=retry {
        last = signin_request(state, agent, jwt, user_id);
        if last.0 || last.2.is_some() {
            return last;
        }
        if attempt < retry {
            std::thread::sleep(RETRY_GAP);
        }
    }
    last
}

// ── 错误分类 / 冷却（对齐 python classify_error / save_cooldown）───────────

/// 根据 HTTP 状态码和业务码分类签到错误，返回 (error_type, cooldown_seconds)。
/// cooldown_seconds: -1=永久, 0=不冷却(仅记录错误计数), >0=冷却秒数
fn classify_error(http_status: i32, code: Option<i64>) -> (&'static str, i64) {
    if http_status == 200 && code == Some(1005) {
        return ("PlanLimit", 43_200);
    }
    if http_status == 429 {
        return ("SoftRate", 60);
    }
    if http_status == 401 {
        return ("SessionDead", -1);
    }
    if http_status == 404 {
        return ("NotFound", 60);
    }
    if (500..600).contains(&http_status) {
        return ("Server", 600);
    }
    if (400..500).contains(&http_status) {
        return ("Client", 600);
    }
    if code.is_some_and(|c| c != 0) {
        return ("BusinessError", 300);
    }
    ("Unknown", 0)
}

/// 写入/更新账号冷却状态到 account_cooldowns.json（语义对齐 python save_cooldown）。
/// Server/Client 类错误前 2 次仅计数不冷却（until=0），第 3 次起进入冷却。
fn save_cooldown(state: &AppState, uid: &str, error_type: &str, cooldown_seconds: i64, reason: &str) {
    let path = state.path("account_cooldowns.json");
    let mut data: AccountCooldownsFile = fs_utils::read_json(&path);
    if cooldown_seconds == 0 {
        data.cooldowns.remove(uid);
        data.updated_at = Some(fs_utils::now_iso());
        let _ = fs_utils::write_json(&path, &data);
        return;
    }
    let entry = if cooldown_seconds == -1 {
        CooldownEntry { error_type: error_type.into(), until: 9_999_999_999, reason: reason.into(), error_count: 0 }
    } else if error_type == "Server" || error_type == "Client" {
        let count = data.cooldowns.get(uid).map(|e| e.error_count).unwrap_or(0) + 1;
        if count < 3 {
            CooldownEntry { error_type: error_type.into(), until: 0, reason: reason.into(), error_count: count }
        } else {
            CooldownEntry {
                error_type: error_type.into(),
                until: chrono::Utc::now().timestamp() + cooldown_seconds,
                reason: reason.into(),
                error_count: 0,
            }
        }
    } else {
        CooldownEntry {
            error_type: error_type.into(),
            until: chrono::Utc::now().timestamp() + cooldown_seconds,
            reason: reason.into(),
            error_count: 0,
        }
    };
    data.cooldowns.insert(uid.to_string(), entry);
    data.updated_at = Some(fs_utils::now_iso());
    let _ = fs_utils::write_json(&path, &data);
}

/// 清除账号冷却状态（签到成功后调用）
fn clear_cooldown(state: &AppState, uid: &str) {
    save_cooldown(state, uid, "", 0, "");
}

// ── 积分归属三层兜底（纯函数，便于单测）────────────────────────────────────

/// python bool() 语义（0/""/null/false → false，其余 true）
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(true, |f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// 宽容整数取数（对齐 python int/整值 float/纯数字串，排除 bool 与负值）
fn int_candidate(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                (i >= 0).then_some(i)
            } else {
                let f = n.as_f64()?;
                (f >= 0.0 && f.fract() == 0.0).then_some(f as i64)
            }
        }
        Value::String(s) => s.trim().parse::<i64>().ok().filter(|i| *i >= 0),
        _ => None,
    }
}

/// 从 claim 响应 JSON 中提取本次签到奖励值（纯函数）。
/// 依次在 data 层与顶层查找候选奖励字段，仅接受整数型数值；无可用字段返回 None。
fn parse_claim_reward(data: Option<&Value>) -> Option<i64> {
    let d = data?;
    let mut scopes: Vec<&Value> = Vec::with_capacity(2);
    if let Some(nested) = d.get("data").filter(|v| v.is_object()) {
        scopes.push(nested);
    }
    scopes.push(d);
    for scope in scopes {
        for key in CLAIM_REWARD_KEYS {
            if let Some(val) = scope.get(key) {
                if let Some(n) = int_candidate(val) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// 签到成功后解析 (final_credits, final_delta, source)（纯函数，recheck 可注入）。
/// 层1 claim 响应奖励字段优先：命中即作 delta，credits 沿用签到前余额；
/// 层2 claim 无奖励字段 → 复查 status 取当前余额，delta = 复查 - 签到前
///（credits_before 缺失不做复查；delta 为负或复查失败落入层3）；
/// 层3 保底旧行为：delta = credits_before、final_credits = credits_before。
/// source ∈ claim_reward / balance_diff / legacy，仅用于日志说明来源。
fn resolve_claim_credits(
    claim_data: Option<&Value>,
    credits_before: Option<i64>,
    mut recheck: Option<&mut dyn FnMut() -> (bool, Option<i64>)>,
) -> (Option<i64>, i64, &'static str) {
    // 层1：claim 响应奖励字段
    if let Some(reward) = parse_claim_reward(claim_data) {
        return (credits_before, reward, "claim_reward");
    }
    // 层2：签到后复查 status，用两次余额差求 delta（失败可容忍，不影响签到成功判定）
    if let (Some(before), Some(re)) = (credits_before, recheck.as_deref_mut()) {
        let (ok_r, credits_after) = re();
        if ok_r && credits_after.is_some_and(|c| c - before >= 0) {
            let after = credits_after.unwrap();
            return (Some(after), after - before, "balance_diff");
        }
    }
    // 层3：保底维持旧行为
    match credits_before {
        Some(c) => (Some(c), c, "legacy"),
        None => (None, 0, "legacy"),
    }
}

/// 把账号最新积分与本次新增写入 credits_history.json（按日期追加，90 天滚动裁剪）。
/// 前端积分看板/趋势图消费此文件。
fn save_credits_history(state: &AppState, user_id: &str, credits: i64, delta: i64) {
    let path = state.path("credits_history.json");
    let mut data: Value = fs_utils::read_json(&path);
    if !data.is_object() {
        data = json!({"records": []});
    }
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    {
        let Some(recs) = data.get_mut("records").and_then(Value::as_array_mut) else {
            data["records"] = json!([{
                "date": today, "user_id": user_id, "credits": credits, "delta": delta,
            }]);
            let _ = fs_utils::write_json(&path, &data);
            return;
        };
        recs.push(json!({"date": today, "user_id": user_id, "credits": credits, "delta": delta}));
        let cutoff = (chrono::Local::now().date_naive() - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        recs.retain(|r| r.get("date").and_then(Value::as_str).unwrap_or("") >= cutoff.as_str());
    }
    let _ = fs_utils::write_json(&path, &data);
}

/// 保存签到结果摘要（同日合并，对齐 python save_summary_merged）：本轮未覆盖的账号
///（被桌面端跳过的已签账号）沿用当日旧记录，避免第二轮整份覆盖丢失「今日已签」状态。
/// 去重键 user_id 优先（同名账号不互吞）；旧记录无 user_id 时回退按 name 匹配。
fn save_summary_merged(state: &AppState, results: Vec<Value>, warnings: &[String]) {
    let path = state.path("checkin_summary.json");
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut merged = results;
    let old: Value = fs_utils::read_json(&path);
    let old_time = old.get("time").and_then(Value::as_str).unwrap_or("");
    if let Some(old_results) = old.get("results").and_then(Value::as_array) {
        if old_time.starts_with(&today) {
            // 收集为owned串：new_uids/new_names 不能借用 merged（下方 push 需可变借用）
            let new_uids: HashSet<String> = merged
                .iter()
                .filter_map(|r| r.get("user_id").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            let new_names: HashSet<String> = merged
                .iter()
                .filter_map(|r| r.get("name").and_then(Value::as_str))
                .map(|s| s.to_string())
                .collect();
            for r in old_results {
                let uid = r.get("user_id").and_then(Value::as_str).unwrap_or("");
                let name = r.get("name").and_then(Value::as_str).unwrap_or("");
                let superseded = if !uid.is_empty() {
                    new_uids.contains(uid)
                } else {
                    new_names.contains(name)
                };
                if !superseded {
                    merged.push(r.clone());
                }
            }
        }
    }
    let total_ok = merged.iter().filter(|r| r.get("ok") == Some(&json!(true))).count();
    let already = merged
        .iter()
        .filter(|r| r.get("action") == Some(&json!("skip_already")))
        .count();
    let failed = merged.iter().filter(|r| r.get("ok") != Some(&json!(true))).count();
    let summary = json!({
        "time": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "results": merged,
        "total_ok": total_ok,
        "already": already,
        "failed": failed,
        "warnings": warnings,
    });
    let _ = fs_utils::write_json(&path, &summary);
}

// ── 单轮主流程（对齐 python main 循环体）───────────────────────────────────

/// 执行一轮签到：逐账号串行处理，事件经 emit 回调逐条输出（NDJSON 管线复用），
/// 返回 done 事件（ok/already/failed 计数）。单账号失败不中断整轮。
/// 侧写全部落盘：冷却/积分历史/摘要/checkin.log 摘要行。
pub fn run_round(
    state: &AppState,
    accounts: &[RawAccount],
    retry: u32,
    emit: &mut dyn FnMut(&Value),
) -> Value {
    let agent = http_agent(30);
    let mut outcome = RoundOutcome::default();
    let mut results: Vec<Value> = Vec::with_capacity(accounts.len());
    let mut warnings: Vec<String> = Vec::new();
    let mut total_ok = 0usize;
    let mut already = 0usize;
    let mut failed = 0usize;

    emit(&json!({"type": "start", "total": accounts.len()}));

    for (i, acc) in accounts.iter().enumerate() {
        let idx = i + 1; // 1-based（前端 next[index-1] 定位行）
        let name = if acc.name.is_empty() { format!("账号{idx}") } else { acc.name.clone() };

        // 未配置 jwt：直接失败（对齐 python 分支）
        let jwt = acc.jwt.trim().to_string();
        if jwt.is_empty() {
            failed += 1;
            let uid = acc.user_id.clone().unwrap_or_default();
            results.push(json!({
                "name": name, "user_id": uid, "ok": false, "message": "未配置 jwt",
            }));
            emit(&json!({
                "type": "account", "index": idx, "user_id": uid, "name": name,
                "status": "fail", "message": "未配置 jwt",
            }));
            outcome.statuses.insert(uid, "fail");
            continue;
        }

        let info = jwt::parse(&jwt);
        let uid_str = info.user_id.clone().unwrap_or_default();

        // JWT 过期告警（摘要 warnings；影响续期提醒，不阻塞签到）
        if let (Some(h), Some(exp)) = (info.exp_hours, info.exp_timestamp) {
            let exp_txt = chrono::Local
                .timestamp_opt(exp, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| exp.to_string());
            if h < 0.0 {
                warnings.push(format!("{name}: JWT 已过期({exp_txt})，请重新抓取"));
            } else if h < EXPIRY_WARN_HOURS {
                warnings.push(format!("{name}: JWT 将于 {h:.1}h 后过期({exp_txt})，请重新抓取"));
            }
        }

        // status 预检：已签跳过 claim（credits 供展示与积分历史落盘）
        let pre = status_check(state, &agent, &jwt, info.user_id.as_deref());
        if pre.ok && pre.checked_in == Some(true) {
            already += 1;
            results.push(json!({
                "name": name, "user_id": info.user_id, "ok": true, "code": 0,
                "action": "skip_already", "credits": pre.credits,
                "message": if pre.message.is_empty() { "已签到" } else { pre.message.as_str() },
            }));
            emit(&json!({
                "type": "account", "index": idx, "user_id": info.user_id, "name": name,
                "status": "already", "credits": pre.credits,
            }));
            if let Some(c) = pre.credits {
                save_credits_history(state, &uid_str, c, 0);
            }
            outcome.statuses.insert(uid_str, "already");
            continue;
        }

        // claim（网络异常按 retry 重试）
        let (ok, msg, code, http_status, claim_data) =
            signin_with_retry(state, &agent, &jwt, info.user_id.as_deref(), retry);
        let mut final_credits: Option<i64> = None;
        let mut final_delta: i64 = 0;
        let mut emit_error_type: Option<&'static str> = None;
        let mut emit_cooldown_until: Option<i64> = None;

        if ok {
            // 签到成功 → 清除冷却 + 积分三层兜底（复查 status 失败可容忍）
            clear_cooldown(state, &uid_str);
            let mut recheck = || {
                let r = status_check(state, &agent, &jwt, info.user_id.as_deref());
                (r.ok, r.credits)
            };
            let (credits, delta, _src) =
                resolve_claim_credits(claim_data.as_ref(), pre.credits, Some(&mut recheck));
            final_credits = credits;
            final_delta = delta;
            if let Some(c) = final_credits {
                save_credits_history(state, &uid_str, c, final_delta);
            }
            total_ok += 1;
        } else {
            // 签到失败 → 分类错误并写入冷却（Unknown 不落盘不外发）
            let (error_type, cooldown_secs) = classify_error(http_status, code);
            if error_type != "Unknown" {
                save_cooldown(state, &uid_str, error_type, cooldown_secs, &msg);
                emit_error_type = Some(error_type);
                let cd: AccountCooldownsFile = fs_utils::read_json(&state.path("account_cooldowns.json"));
                emit_cooldown_until = cd.cooldowns.get(&uid_str).map(|e| e.until).or(Some(0));
                outcome.error_types.insert(uid_str.clone(), error_type.to_string());
            }
            failed += 1;
        }
        outcome.statuses.insert(uid_str.clone(), if ok { "success" } else { "fail" });

        results.push({
            let mut r = json!({
                "name": name, "user_id": info.user_id, "ok": ok, "code": code,
                "message": msg, "action": if ok { "claim_ok" } else { "claim" },
            });
            if let Some(c) = final_credits {
                r["credits"] = json!(c);
                r["credits_delta"] = json!(final_delta);
            }
            r
        });
        emit(&json!({
            "type": "account", "index": idx, "user_id": info.user_id, "name": name,
            "status": if ok { "success" } else { "fail" },
            "code": code, "message": msg,
            "credits": final_credits,
            "delta": if final_delta != 0 { Some(final_delta) } else { None },
            "error_type": emit_error_type,
            "cooldown_until": emit_cooldown_until,
        }));
    }

    // 摘要行写入 logs/checkin.log（python 同款；告警一并落 app 日志）
    let now_txt = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut log_line = format!(
        "[{now_txt}] 成功{total_ok}/已签到{already}/失败{failed}/总计{}",
        accounts.len()
    );
    if !warnings.is_empty() {
        log_line.push_str(&format!(" | 告警: {}", warnings.join("; ")));
        fs_utils::app_log(&state.data_dir, &format!("签到告警: {}", warnings.join("; ")));
    }
    log_line.push('\n');
    let log_path = state.data_dir.join("logs").join("checkin.log");
    if let Some(p) = log_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        use std::io::Write as _;
        let _ = f.write_all(log_line.as_bytes());
    }

    save_summary_merged(state, results, &warnings);
    let done = json!({"type": "done", "ok": total_ok, "already": already, "failed": failed});
    emit(&done);
    done
}

// ── 单元测试（纯函数层）────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_error_按状态码与业务码分类() {
        assert_eq!(classify_error(200, Some(1005)), ("PlanLimit", 43_200));
        assert_eq!(classify_error(429, None), ("SoftRate", 60));
        assert_eq!(classify_error(401, None), ("SessionDead", -1));
        assert_eq!(classify_error(404, None), ("NotFound", 60));
        assert_eq!(classify_error(502, None), ("Server", 600));
        assert_eq!(classify_error(403, None), ("Client", 600));
        assert_eq!(classify_error(200, Some(7)), ("BusinessError", 300));
        // 传输层异常（status=0, code=None）→ Unknown 不冷却
        assert_eq!(classify_error(0, None), ("Unknown", 0));
    }

    #[test]
    fn parse_claim_reward_候选字段按优先级() {
        assert_eq!(parse_claim_reward(Some(&json!({"reward": 10}))), Some(10));
        // data 层优先于顶层
        assert_eq!(
            parse_claim_reward(Some(&json!({"data": {"delta": 5}, "credits": 99}))),
            Some(5)
        );
        assert_eq!(parse_claim_reward(Some(&json!({"delta": "8"}))), Some(8));
        // 整值 float 接受、小数/负值/bool 拒绝
        assert_eq!(parse_claim_reward(Some(&json!({"delta": 3.0}))), Some(3));
        assert_eq!(parse_claim_reward(Some(&json!({"delta": 3.5}))), None);
        assert_eq!(parse_claim_reward(Some(&json!({"delta": -1}))), None);
        assert_eq!(parse_claim_reward(Some(&json!({"reward": true}))), None);
        assert_eq!(parse_claim_reward(Some(&json!({"other": 1}))), None);
        assert_eq!(parse_claim_reward(None), None);
    }

    #[test]
    fn resolve_claim_credits_三层兜底顺序() {
        // 层1：claim 奖励字段命中 → credits 沿用签到前余额
        let (c, d, s) = resolve_claim_credits(Some(&json!({"reward": 5})), Some(100), None::<&mut dyn FnMut() -> (bool, Option<i64>)>);
        assert_eq!((c, d, s), (Some(100), 5, "claim_reward"));
        // 层2：无奖励字段 → 复查余额差值（正差采信）
        let mut re = || (true, Some(105i64));
        let (c, d, s) = resolve_claim_credits(Some(&json!({})), Some(100), Some(&mut re));
        assert_eq!((c, d, s), (Some(105), 5, "balance_diff"));
        // 层2 负差 → 落层3
        let mut re = || (true, Some(90i64));
        let (c, d, s) = resolve_claim_credits(Some(&json!({})), Some(100), Some(&mut re));
        assert_eq!((c, d, s), (Some(100), 100, "legacy"));
        // 层3：签到前余额缺失 → (None, 0)
        let (c, d, s) = resolve_claim_credits(Some(&json!({})), None, None::<&mut dyn FnMut() -> (bool, Option<i64>)>);
        assert_eq!((c, d, s), (None, 0, "legacy"));
    }

    #[test]
    fn int_candidate_宽容整数语义() {
        assert_eq!(int_candidate(&json!(12)), Some(12));
        assert_eq!(int_candidate(&json!(12.0)), Some(12));
        assert_eq!(int_candidate(&json!("30")), Some(30));
        assert_eq!(int_candidate(&json!(true)), None);
        assert_eq!(int_candidate(&json!(-5)), None);
        assert_eq!(int_candidate(&json!(null)), None);
    }

    #[test]
    fn py_truthy_对齐_python_bool() {
        assert!(!py_truthy(&json!(0)));
        assert!(py_truthy(&json!(1)));
        assert!(!py_truthy(&json!("")));
        assert!(!py_truthy(&json!(null)));
        assert!(py_truthy(&json!("0"))); // python bool("0") == True
    }
}
