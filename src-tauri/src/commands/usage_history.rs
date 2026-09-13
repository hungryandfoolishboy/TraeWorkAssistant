//! Trae Work 积分消耗历史（F-27+：`POST /trae/api/v1/pay/query_user_usage_group_by_session`）。
//!
//! 此前积分看板的「消耗」口径是 credits_daily 快照的余额差值推算（含签到获得等噪声）；
//! 本模块改为直连接口拉取会话级用量（credits_float / model_name / token 明细），按本地
//! 自然日聚合后落盘 data/usage_history.json（结果级 10 分钟 TTL + stale-on-error 沿用
//! 缓存），供积分看板查询展示。
//!
//! 请求形态（2026-09-13 代理日志实测）：
//! `{"start_time":<unix秒>,"end_time":<unix秒>,"page_size":N,"page_num":1,"usage_type":[7]}`
//! 响应：`{"total":<会话总数>,"user_usage_group_by_sessions":[{usage_time, credits_float,
//! model_name, extra_info:{input_token,output_token,cache_read_token}, ...}]}`
//!
//! 凭证红线：JWT 仅进请求头（复用 ide_query_post），不进日志/返回值。

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;
use tauri::State;

use crate::state::AppState;

/// 结果缓存 TTL（秒）：进程内结果缓存 + 落盘持久化（跨重启可查）
const RESULT_TTL_SECS: u64 = 600;
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
    pub error: Option<String>,
    pub sessions: u64,
    pub credits: f64,
    /// 按日期升序
    pub daily: Vec<UsageDayStat>,
}

#[derive(Serialize, Clone)]
pub struct UsageHistoryResult {
    pub fetched_at: i64,
    pub days: u32,
    /// true = 命中缓存（含 stale-on-error 沿用）
    pub cached: bool,
    pub accounts: Vec<UsageHistoryAccount>,
    pub total_credits: f64,
    pub total_sessions: u64,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CachedAccount {
    name: String,
    daily: BTreeMap<String, UsageDayStat>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CacheFile {
    fetched_at: Option<i64>,
    days: Option<u32>,
    accounts: BTreeMap<String, CachedAccount>,
}

fn cache_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join("data").join("usage_history.json")
}

static RESULT_CACHE: Mutex<Option<(Instant, UsageHistoryResult)>> = Mutex::new(None);

fn daily_to_vec(daily: &BTreeMap<String, UsageDayStat>) -> Vec<UsageDayStat> {
    daily.values().cloned().collect()
}

fn account_summary(name: String, uid: String, daily: &BTreeMap<String, UsageDayStat>) -> UsageHistoryAccount {
    let credits = daily.values().map(|d| d.credits).sum();
    let sessions = daily.values().map(|d| d.sessions).sum();
    UsageHistoryAccount {
        user_id: uid,
        name,
        ok: true,
        error: None,
        sessions,
        credits,
        daily: daily_to_vec(daily),
    }
}

/// 单账号分页拉取并按本地日聚合。start/end 为 Unix 秒；失败返回 Err（调用方沿用缓存）。
fn fetch_account_usage(
    state: &AppState,
    uid: &str,
    jwt: &str,
    days: u32,
) -> Result<BTreeMap<String, UsageDayStat>, String> {
    let agent = crate::commands::accounts::pay_status_agent();
    let dev = crate::commands::accounts::resolve_device(state, uid);
    let end = chrono::Local::now().timestamp();
    let start = end - (days as i64 - 1) * 86400;

    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut page: u32 = 1;
    let mut got: usize = 0;
    let mut total: Option<usize> = None;

    loop {
        let body = json!({
            "start_time": start,
            "end_time": end,
            "page_size": PAGE_SIZE,
            "page_num": page,
            "usage_type": [USAGE_TYPE],
        });
        let resp = crate::commands::accounts::ide_query_post(
            &agent,
            USAGE_URL,
            jwt,
            &dev,
            body,
        )?;
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
            let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) else {
                continue;
            };
            let date = dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
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
/// async 派发：逐账号串行分页网络请求（每请求最长 60s），同步命令会冻住 UI。
#[tauri::command(async)]
pub fn usage_history_fetch(
    state: State<AppState>,
    days: Option<u32>,
    fresh: Option<bool>,
) -> Result<UsageHistoryResult, String> {
    let days = days.unwrap_or(30).clamp(1, 90);
    let fresh = fresh.unwrap_or(false);
    let now_ts = chrono::Local::now().timestamp();

    // ① 进程内结果缓存
    if !fresh {
        if let Ok(guard) = RESULT_CACHE.lock() {
            if let Some((at, v)) = guard.as_ref() {
                if at.elapsed().as_secs() < RESULT_TTL_SECS && v.days == days {
                    return Ok(v.clone());
                }
            }
        }
        // ② 落盘缓存（跨重启可查；窗口一致且未过期直接复用）
        let cache: CacheFile = crate::fs_utils::read_json(&cache_path(&state));
        if cache.days == Some(days)
            && cache
                .fetched_at
                .map(|t| now_ts - t < RESULT_TTL_SECS as i64)
                .unwrap_or(false)
        {
            let accounts = crate::vault::load_accounts(&state);
            let mut out = Vec::new();
            for a in &accounts.accounts {
                let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
                    continue;
                };
                if let Some(c) = cache.accounts.get(&uid) {
                    out.push(account_summary(c.name.clone(), uid, &c.daily));
                }
            }
            let total_credits = out.iter().map(|a| a.credits).sum();
            let total_sessions = out.iter().map(|a| a.sessions).sum();
            let result = UsageHistoryResult {
                fetched_at: cache.fetched_at.unwrap_or(now_ts),
                days,
                cached: true,
                accounts: out,
                total_credits,
                total_sessions,
            };
            if let Ok(mut guard) = RESULT_CACHE.lock() {
                *guard = Some((Instant::now(), result.clone()));
            }
            return Ok(result);
        }
    }

    // ③ 逐账号拉取（无凭证/失败的账号沿用其缓存，stale-on-error）
    let accounts = crate::vault::load_accounts(&state);
    let old_cache: CacheFile = crate::fs_utils::read_json(&cache_path(&state));
    let mut out = Vec::new();
    let mut new_cache = CacheFile {
        fetched_at: Some(now_ts),
        days: Some(days),
        accounts: BTreeMap::new(),
    };
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        if a.jwt.trim().is_empty() {
            // 占位账号（无 JWT）：沿用缓存或如实标记
            match old_cache.accounts.get(&uid) {
                Some(c) => out.push(account_summary(name, uid, &c.daily)),
                None => out.push(UsageHistoryAccount {
                    user_id: uid,
                    name,
                    ok: false,
                    error: Some("账号无 JWT 凭证，无法查询消耗明细".into()),
                    ..Default::default()
                }),
            }
            continue;
        }
        match fetch_account_usage(&state, &uid, &a.jwt, days) {
            Ok(daily) => {
                new_cache.accounts.insert(
                    uid.clone(),
                    CachedAccount {
                        name: name.clone(),
                        daily: daily.clone(),
                    },
                );
                out.push(account_summary(name, uid, &daily));
            }
            Err(e) => match old_cache.accounts.get(&uid) {
                // stale-on-error：沿用上一次成功数据并标记缓存来源
                Some(c) => {
                    let mut acc = account_summary(name, uid, &c.daily);
                    acc.error = Some(format!("本次查询失败（展示上次缓存）：{e}"));
                    out.push(acc);
                }
                None => out.push(UsageHistoryAccount {
                    user_id: uid,
                    name,
                    ok: false,
                    error: Some(e),
                    ..Default::default()
                }),
            },
        }
    }

    let total_credits = out.iter().map(|a| a.credits).sum();
    let total_sessions = out.iter().map(|a| a.sessions).sum();
    let result = UsageHistoryResult {
        fetched_at: now_ts,
        days,
        cached: false,
        accounts: out,
        total_credits,
        total_sessions,
    };
    let _ = crate::fs_utils::write_json(&cache_path(&state), &new_cache);
    if let Ok(mut guard) = RESULT_CACHE.lock() {
        *guard = Some((Instant::now(), result.clone()));
    }
    Ok(result)
}
