//! 应用内定时调度器（Rust 原生方案，补充 Windows schtasks 计划任务）。
//!
//! ## 背景（依赖分析结论）
//!
//! 工程内的每日/每周业务任务此前 **100% 依赖 Windows schtasks 注册**：
//! 各设置页 `/SC DAILY|WEEKLY` 注册任务名（AIWorkAssistant_DailyCheckin、
//! AIWorkAssistant_WorkBuddyCheckin_<HHMM>、AIWorkAssistant_WorkBuddyRenew、
//! AIWorkAssistant_DoubaoRenew、AIWorkAssistant_DoubaoQuotaCheck），
//! 触发时直调主 exe CLI 任务模式（`--task-run <name>`）。缺口：
//!
//! 1. schtasks 未注册 / 注册失败（如权限不足 Access Denied）时，应用开着
//!    也不会跑任何每日任务；
//! 2. 积分余额快照（Trae `credits_daily.json` / WorkBuddy
//!    `workbuddy_credits_history.json`）此前无任何定时写入——只在打开积分页
//!    且非缓存命中时落盘，未打开应用的日子快照缺天，「近 7 日积分消耗」
//!    （快照差分口径）因此只剩昨天一格。
//!
//! ## 语义
//!
//! - 单后台线程 60s tick；每个任务有默认触发时刻（HH:MM，对齐 schtasks 典型时段）；
//! - 触发条件：`今天已过触发时刻 && 状态文件 last_run_date != 今天` → 执行；
//!   应用在触发时刻之后才启动也会补跑一次（启动补跑，等价每日一次语义）；
//! - 失败重试：仅成功才记 last_run_date；失败记 last_fail_ts，30 分钟冷却后
//!   自动重试（避免一次网络抖动丢掉整天的签到/快照，也避免每 tick 连打）；
//! - 所有任务实现均幂等（签到 skip-checked / 快照同日覆盖），与 schtasks
//!   重复触发不会产生重复效果；
//! - 串行执行：一轮 tick 内任务逐个跑完，WB 签到复用轮次锁与 UI 路径互斥。
//!
//! ## 状态
//!
//! `scheduler_state.json`：
//! `{ "tasks": { "<key>": { "last_run_date", "last_run_ts", "last_ok", "last_fail_ts", "last_summary" } } }`
//! 前端经 `scheduler_status` 命令查看各任务最近一次执行情况。

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::fs_utils;
use crate::state::AppState;

/// 调度任务定义（默认触发时刻为本地时间 HH:MM）
struct SchedTask {
    key: &'static str,
    name: &'static str,
    /// 默认触发时刻 HH:MM
    hhmm: &'static str,
}

const TASKS: &[SchedTask] = &[
    SchedTask { key: "trae-checkin", name: "Trae 每日签到", hhmm: "09:00" },
    SchedTask { key: "wb-checkin", name: "WorkBuddy 每日签到", hhmm: "09:10" },
    // F-09 兜底续期：lazy 24h（到期前 24h 内才真正刷新），每天跑一次是安全超集
    SchedTask { key: "wb-renew", name: "WorkBuddy token 兜底续期", hhmm: "10:30" },
    SchedTask { key: "doubao-keepalive", name: "豆包会话每日续期", hhmm: "09:20" },
    SchedTask { key: "doubao-quota", name: "豆包会员额度每日巡检", hhmm: "09:30" },
    // 快照类排到晚間（接近日末，差分口径最准）；此前无定时写入，是近 7 日消耗缺天的根因
    SchedTask { key: "wb-credits-snapshot", name: "WorkBuddy 积分余额每日快照", hhmm: "23:30" },
    SchedTask { key: "trae-credits-snapshot", name: "Trae 积分余额每日快照", hhmm: "23:40" },
];

/// 启动调度线程（main.rs setup 调用；启动 90s 后首跑，避开启动高峰）
pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        loop {
            let st = app.state::<AppState>();
            tick(&st);
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    });
}

/// 失败重试冷却：失败后 30 分钟内不重复尝试（避免每 60s tick 连打）
const RETRY_COOLDOWN_MS: i64 = 30 * 60_000;

/// 单轮 tick：检查每个任务「今天已过触发时刻 && 今天未跑」→ 执行
fn tick(st: &AppState) {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let now_hm = now.format("%H:%M").to_string();
    for t in TASKS {
        // 各任务启用判定（与既有设置语义保持一致）
        if !enabled(st, t.key) {
            continue;
        }
        if last_run_date(st, t.key).as_deref() == Some(today.as_str()) {
            continue;
        }
        // HH:MM 零填充，字符串比较即时间序
        if now_hm.as_str() < t.hhmm {
            continue;
        }
        // 失败冷却：30 分钟内静默等待重试，不重复执行也不刷日志
        if let Some(ts) = last_fail_ts(st, t.key) {
            if chrono::Utc::now().timestamp_millis() - ts < RETRY_COOLDOWN_MS {
                continue;
            }
        }
        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_task(t.key, st))) {
            Ok(Ok(v)) => Ok(summarize(&v)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("任务线程 panic（已捕获，不影响后续调度）".to_string()),
        };
        match outcome {
            Ok(summary) => {
                mark_run(st, t.key, &today, &summary);
                fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（每日 {}）：{}", t.name, t.hhmm, summary));
            }
            Err(summary) => {
                mark_fail(st, t.key, &summary);
                fs_utils::app_log(&st.data_dir, &format!("[调度器] {}（每日 {}）失败，30 分钟后重试：{}", t.name, t.hhmm, summary));
            }
        }
    }
}

/// 任务启用判定：快照/巡检/续期类恒开（补齐数据时序），签到类跟随各自设置开关
fn enabled(st: &AppState, key: &str) -> bool {
    match key {
        // WorkBuddy 签到跟随「启动自动补签」开关（F-55 同源设置）
        "wb-checkin" => crate::commands::workbuddy::wb_auto_checkin_enabled(st),
        // 其余任务幂等且低风险，恒开（Trae 签到 run_round 自带状态核验）
        _ => true,
    }
}

/// 执行单个任务（复用 CLI 任务同款实现，进度静默、结果汇总落日志）
fn run_task(key: &str, st: &AppState) -> Result<Value, String> {
    match key {
        // Trae 每日签到：与 `--task-run checkin` 同款（vault 全账号单轮，状态核验幂等）
        "trae-checkin" => {
            let accounts = crate::vault::load_accounts(st);
            let retry = st.settings().retry.max(0) as u32;
            Ok(super::trae_checkin::run_round(st, &accounts.accounts, retry, &mut |_| {}))
        }
        // WorkBuddy 每日签到：与 `--task-run wb-checkin` 同款；抢轮次锁与 UI 路径互斥
        "wb-checkin" => {
            let Ok(_round) = crate::commands::workbuddy::try_acquire_wb_round() else {
                return Err("跳过：已有签到/成长任务在执行中".into());
            };
            Ok(super::wb_checkin::run_checkin_round(st, &super::wb_checkin::CheckinOpts::daily(), &mut |_| {}))
        }
        // WorkBuddy token 兜底续期：与 `--task-run wb-renew` 同款（lazy 24h）
        "wb-renew" => Ok(super::wb_checkin::run_renew_only(st, 24)),
        // 豆包会话每日续期：与 `--task-run doubao-keepalive` 同款
        "doubao-keepalive" => {
            let sink = crate::switcher::CliSink::new(&st.data_dir);
            crate::switcher::run_action(
                crate::switcher::RunArgs {
                    action: crate::switcher::Action::KeepAlive,
                    target_app: crate::switcher::TargetApp::Doubao,
                    user_id: None,
                    proxy_port: None,
                    include_indexeddb: false,
                    expected_current_uid: String::new(),
                    data_dir: st.data_dir.clone(),
                },
                &sink,
            )
            .map(|_| json!({ "ok": true }))
        }
        // 豆包会员额度每日巡检：与 `--task-run doubao-quota` 同款
        "doubao-quota" => super::doubao_quota::run_batch(st),
        // WorkBuddy 积分余额每日快照（新增任务）：补齐近 7 日消耗差分时序
        "wb-credits-snapshot" => crate::commands::workbuddy::wb_credits_snapshot_task(st)
            .map(|p| json!({ "ok": true, "accounts": p.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) })),
        // Trae 积分余额每日快照（新增任务）：与 `--task-run refresh-credits` 同款
        "trae-credits-snapshot" => crate::commands::accounts::refresh_remaining_credits_impl(st)
            .map(|n| json!({ "ok": true, "refreshed": n })),
        other => Err(format!("未知调度任务: {other}")),
    }
}

// ── 状态持久化 ───────────────────────────────────────────────────────────────

fn state_path(st: &AppState) -> std::path::PathBuf {
    st.path("scheduler_state.json")
}

fn load_state(st: &AppState) -> Value {
    let v: Value = fs_utils::read_json(&state_path(st));
    if v.is_object() { v } else { json!({}) }
}

fn last_run_date(st: &AppState, key: &str) -> Option<String> {
    load_state(st)
        .pointer(&format!("/tasks/{key}/last_run_date"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn last_fail_ts(st: &AppState, key: &str) -> Option<i64> {
    load_state(st)
        .pointer(&format!("/tasks/{key}/last_fail_ts"))
        .and_then(Value::as_i64)
}

/// 成功：记「今天已跑」（整体覆盖条目，同时清掉 last_fail_ts）
fn mark_run(st: &AppState, key: &str, date: &str, summary: &str) {
    write_entry(
        st,
        key,
        json!({
            "last_run_date": date,
            "last_run_ts": chrono::Utc::now().timestamp_millis(),
            "last_ok": true,
            "last_summary": summary,
        }),
    );
}

/// 失败：只记失败时间戳，不写 last_run_date（冷却后当天自动重试）
fn mark_fail(st: &AppState, key: &str, summary: &str) {
    write_entry(
        st,
        key,
        json!({
            "last_fail_ts": chrono::Utc::now().timestamp_millis(),
            "last_ok": false,
            "last_summary": summary,
        }),
    );
}

fn write_entry(st: &AppState, key: &str, entry: Value) {
    let mut root = load_state(st);
    if let Some(obj) = root.as_object_mut() {
        let tasks = obj.entry("tasks".to_string()).or_insert_with(|| json!({}));
        tasks[key] = entry;
    }
    let _ = fs_utils::write_json(&state_path(st), &root);
}

/// 结果 JSON → 单行摘要（签到轮次取 ok/already/failed 计数，其余截断展示）
fn summarize(v: &Value) -> String {
    if v.is_object() {
        let get = |k: &str| v.get(k).and_then(Value::as_i64);
        if let (Some(o), Some(a), Some(f)) = (get("ok"), get("already"), get("failed")) {
            return format!("成功 {o}，已签 {a}，失败 {f}");
        }
    }
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(120).collect()
}

/// 调度器状态查询（前端/排查用）：任务定义 + 最近一次执行情况
#[tauri::command]
pub fn scheduler_status(st: tauri::State<AppState>) -> Value {
    let raw = load_state(&st);
    let tasks: Vec<Value> = TASKS
        .iter()
        .map(|t| {
            let e = raw.pointer(&format!("/tasks/{}", t.key)).cloned().unwrap_or(Value::Null);
            json!({
                "key": t.key,
                "name": t.name,
                "time": t.hhmm,
                "last_run_date": e.get("last_run_date").cloned().unwrap_or(Value::Null),
                "last_run_ts": e.get("last_run_ts").cloned().unwrap_or(Value::Null),
                "last_fail_ts": e.get("last_fail_ts").cloned().unwrap_or(Value::Null),
                "last_ok": e.get("last_ok").cloned().unwrap_or(Value::Null),
                "last_summary": e.get("last_summary").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "tasks": tasks })
}
