//! Trae Work 积分消耗历史（`POST /trae/api/v1/pay/query_user_usage_group_by_session`）。
//!
//! 此前积分看板的「消耗」口径是 credits_daily 快照的余额差值推算（含签到获得等噪声）；
//! 本模块改为直连接口拉取会话级用量（credits_float / model_name / token 明细），按本地
//! 自然日聚合落盘 data/usage_history.json，供积分趋势图查询展示。
//!
//! 增量语义（避免重复计数）：
//! - 首次拉取（无缓存）：全量拉取近一年（FULL_PULL_DAYS）；
//! - 后续拉取（fresh=true）：从「上次拉取 end_time 所在本地日的 00:00」起重拉，
//!   并**替换**缓存中该日期及之后的日聚合（当天多次拉取不叠加；更早的历史保持不动）；
//! - fresh=false：纯缓存读取，零网络。
//!
//! 请求形态（2026-09-13 代理日志实测）：
//! `{"start_time":<unix秒>,"end_time":<unix秒>,"page_size":N,"page_num":1,"usage_type":[7]}`
//! 响应：`{"total":<会话总数>,"user_usage_group_by_sessions":[{usage_time, credits_float,
//! model_name, extra_info:{input_token,output_token,cache_read_token}, ...}]}`
//!
//! 凭证红线：JWT 仅进请求头（复用 ide_query_post），不进日志/返回值。

use chrono::TimeZone;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tauri::State;

use crate::state::AppState;

/// 首次全量拉取窗口（天）
const FULL_PULL_DAYS: i64 = 365;
/// 单账号分页安全上限（防 total 异常导致死循环；50 页 × 100 = 5000 会话）
const MAX_PAGES: u32 = 50;
/// 单页大小（服务端实测 20；取大值，实际以返回长度自适应）
const PAGE_SIZE: u32 = 100;
/// 用量类型 7 = Cloud-IDE 会话积分消耗（实测口径）
const USAGE_TYPE: i64 = 7;

const USAGE_URL: &str = "https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session";

/// 单日聚合（date → 消耗合计 / 会话数 / 模型分布 / token 明细）
#[derive(Serialize, serde::Deserialize, Clone, Default)]
pub struct UsageDayStat {
    pub date: String,
    pub credits: f64,
    pub sessions: u64,
    pub models: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageHistoryAccount {
    pub user_id: String,
    pub name: String,
    pub ok: bool,
    /// 本次增量拉取失败但已沿用缓存时的说明；无缓存时为失败原因
    pub error: Option<String>,
    /// 按日期升序
    pub daily: Vec<UsageDayStat>,
}

#[derive(Serialize, Clone)]
pub struct UsageHistoryResult {
    pub fetched_at: i64,
    /// true = 纯缓存读取（未发起网络请求）
    pub cached: bool,
    pub accounts: Vec<UsageHistoryAccount>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CachedAccount {
    name: String,
    /// 上次拉取的 end_time（Unix 秒）——增量起点 = 该时刻所在本地日的 00:00
    last_fetch_end_ts: Option<i64>,
    daily: BTreeMap<String, UsageDayStat>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CacheFile {
    fetched_at: Option<i64>,
    accounts: BTreeMap<String, CachedAccount>,
}

fn cache_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join("data").join("usage_history.json")
}

fn account_summary(name: String, uid: String, daily: &BTreeMap<String, UsageDayStat>) -> UsageHistoryAccount {
    UsageHistoryAccount {
        user_id: uid,
        name,
        ok: true,
        error: None,
        daily: daily.values().cloned().collect(),
    }
}

/// Unix 秒 → 本地自然日（YYYY-MM-DD）
fn local_date_of(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// 本地自然日 → 当日 00:00 的 Unix 秒（本地时区；无效日期回退 None）
fn local_midnight_ts(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    match chrono::Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
    {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

/// 单账号分页拉取 [start_ts, end_ts] 区间并按本地日聚合。失败返回 Err（调用方沿用缓存）。
fn fetch_account_usage(
    state: &AppState,
    uid: &str,
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<BTreeMap<String, UsageDayStat>, String> {
    let agent = crate::commands::accounts::pay_status_agent();
    let dev = crate::commands::accounts::resolve_device(state, uid);

    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut page: u32 = 1;
    let mut got: usize = 0;
    let mut total: Option<usize> = None;

    loop {
        let body = json!({
            "start_time": start_ts,
            "end_time": end_ts,
            "page_size": PAGE_SIZE,
            "page_num": page,
            "usage_type": [USAGE_TYPE],
        });
        let resp =
            crate::commands::accounts::ide_query_post(&agent, USAGE_URL, jwt, &dev, body)?;
        if total.is_none() {
            total = Some(resp.get("total").and_then(Value::as_u64).unwrap_or(0) as usize);
        }
        let arr = resp
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for s in &arr {
            let ts = s.get("usage_time").and_then(Value::as_i64).unwrap_or(0);
            if ts <= 0 {
                continue;
            }
            // usage_time 为 Unix 秒（实测 1789009361），转本地自然日
            let Some(date) = local_date_of(ts) else {
                continue;
            };
            let credits = s
                .get("credits_float")
                .and_then(Value::as_f64)
                .or_else(|| s.get("amount_float").and_then(Value::as_f64))
                .unwrap_or(0.0);
            let model = s
                .get("model_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("未知模型")
                .to_string();
            let e = agg.entry(date).or_default();
            e.credits += credits;
            e.sessions += 1;
            *e.models.entry(model).or_insert(0.0) += credits;
            if let Some(extra) = s.get("extra_info") {
                e.input_tokens += extra.get("input_token").and_then(Value::as_u64).unwrap_or(0);
                e.output_tokens += extra.get("output_token").and_then(Value::as_u64).unwrap_or(0);
                e.cache_read_tokens += extra
                    .get("cache_read_token")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
        }
        got += arr.len();
        let total_n = total.unwrap_or(0);
        // 终止条件：已取满 total / 本页不满页大小（服务端截断页）/ 超过安全页数上限
        if got >= total_n || (arr.len() as u32) < PAGE_SIZE || page >= MAX_PAGES {
            break;
        }
        page += 1;
    }
    Ok(agg)
}

/// 拉取全部账号的积分消耗历史（按本地日聚合），落盘缓存供查询展示。
/// fresh=false：纯缓存读取（零网络）；fresh=true：增量拉取（无缓存账号全量近一年，
/// 已有账号从上次拉取日 00:00 起重拉并替换该日期及之后的聚合）。
/// async 派发：逐账号串行分页网络请求（每请求最长 60s），同步命令会冻住 UI。
#[tauri::command(async)]
pub fn usage_history_fetch(
    state: State<AppState>,
    fresh: Option<bool>,
) -> Result<UsageHistoryResult, String> {
    let fresh = fresh.unwrap_or(false);
    let now_ts = chrono::Local::now().timestamp();
    let accounts = crate::vault::load_accounts(&state);
    let mut cache: CacheFile = crate::fs_utils::read_json(&cache_path(&state));

    // 纯缓存读取（零网络；尚未拉取过的账号如实提示）
    if !fresh {
        let mut out = Vec::new();
        for a in &accounts.accounts {
            let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
                continue;
            };
            match cache.accounts.get(&uid) {
                Some(c) => out.push(account_summary(c.name.clone(), uid, &c.daily)),
                None => out.push(UsageHistoryAccount {
                    user_id: uid,
                    name: a.name.clone(),
                    ok: false,
                    error: Some("尚未拉取消耗明细，点击「更新消耗明细」拉取".into()),
                    ..Default::default()
                }),
            }
        }
        return Ok(UsageHistoryResult {
            fetched_at: cache.fetched_at.unwrap_or(0),
            cached: true,
            accounts: out,
        });
    }

    // 增量拉取：无缓存账号全量近一年；已有账号从上次拉取日 00:00 重拉并替换该日及之后
    let full_start = now_ts - FULL_PULL_DAYS * 86400;
    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        if a.jwt.trim().is_empty() {
            // 占位账号（无 JWT）：保留既有缓存，不发起请求
            continue;
        }
        let (start_ts, refetch_from) =
            match cache.accounts.get(&uid).and_then(|c| c.last_fetch_end_ts) {
                Some(last_end) => {
                    let from_date = local_date_of(last_end)
                        .or_else(|| local_date_of(now_ts))
                        .unwrap_or_default();
                    let midnight = local_midnight_ts(&from_date).unwrap_or(now_ts - 86400);
                    (midnight, from_date)
                }
                None => (full_start, String::new()),
            };
        match fetch_account_usage(&state, &uid, &a.jwt, start_ts, now_ts) {
            Ok(new_agg) => {
                let entry = cache.accounts.entry(uid.clone()).or_default();
                entry.name = name;
                if refetch_from.is_empty() {
                    // 全量：整体替换
                    entry.daily = new_agg;
                } else {
                    // 增量：替换 refetch_from 及之后的日聚合（当天多次拉取不叠加）
                    entry
                        .daily
                        .retain(|d, _| d.as_str() < refetch_from.as_str());
                    for (d, v) in new_agg {
                        entry.daily.insert(d, v);
                    }
                }
                entry.last_fetch_end_ts = Some(now_ts);
            }
            Err(e) => {
                // 拉取失败：保留旧缓存，错误在结果中注明
                errors.insert(uid, e);
            }
        }
    }

    cache.fetched_at = Some(now_ts);
    let _ = crate::fs_utils::write_json(&cache_path(&state), &cache);

    let mut out = Vec::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        match cache.accounts.get(&uid) {
            Some(c) => {
                let mut acc = account_summary(c.name.clone(), uid.clone(), &c.daily);
                if let Some(e) = errors.get(&uid) {
                    acc.error = Some(format!("本次更新失败（展示已有缓存）：{e}"));
                }
                out.push(acc);
            }
            None => out.push(UsageHistoryAccount {
                error: errors.get(&uid).cloned(),
                user_id: uid,
                name,
                ok: false,
                ..Default::default()
            }),
        }
    }
    Ok(UsageHistoryResult {
        fetched_at: now_ts,
        cached: false,
        accounts: out,
    })
}
