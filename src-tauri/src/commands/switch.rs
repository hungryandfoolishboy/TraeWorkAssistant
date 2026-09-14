//! 登录态切换/保存/设备重置命令（switcher 模块的 Tauri 命令封装）。
//! 原实现经 powershell 子进程管道消费 NDJSON；Rust 化后收敛为
//! `switcher::run_action` 进程内直调 + `TauriSink` 一步到位 emit 事件，
//! 终态 `*-done {success, raw}` 由命令层依据返回值发射（前端契约不变）。

use tauri::{AppHandle, Emitter, State};

use crate::fs_utils;
use crate::state::AppState;
use crate::switcher::{Action, RunArgs, TauriSink, TargetApp};

/// 构造切/存/恢复类命令的通用入参
fn build_args(
    action: Action,
    target_app: Option<&str>,
    user_id: Option<String>,
    proxy_port: Option<u16>,
    include_indexeddb: bool,
    expected_current_uid: String,
    data_dir: std::path::PathBuf,
) -> RunArgs {
    RunArgs {
        action,
        target_app: TargetApp::parse(target_app.unwrap_or("TraeWork")),
        user_id,
        proxy_port: proxy_port.filter(|p| *p > 0),
        include_indexeddb,
        expected_current_uid,
        data_dir,
    }
}

/// 后台执行 run_action 并发射终态事件；成功后可选回调（dc id 回填等，携带 data_dir）
fn run_in_background(
    app: AppHandle,
    event_progress: &'static str,
    event_done: &'static str,
    args: RunArgs,
    on_success: Option<Box<dyn FnOnce(&AppHandle, &std::path::Path) + Send>>,
) {
    std::thread::spawn(move || {
        let data_dir = args.data_dir.clone();
        let sink = TauriSink::new(&app, event_progress, &data_dir);
        let result = crate::switcher::run_action(args, &sink);
        let (success, raw) = match &result {
            Ok(line) => (true, line.clone()),
            Err(line) => (false, line.clone()),
        };
        let _ = app.emit(event_done, serde_json::json!({ "success": success, "raw": raw }));
        if success {
            if let Some(cb) = on_success {
                cb(&app, &data_dir);
            }
        }
    });
}

// async：内含会话/JWT 预检子进程（网络 I/O）与守卫 uid 检测子进程，同步命令会冻结 UI
//（项目约定：阻塞型命令一律 #[tauri::command(async)]）
#[tauri::command(async)]
pub fn switch_account(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
    proxy_port: Option<u16>,
    // 续期 JWT 流程专用：目标账号 JWT 本就可能已吊销（续期正是为了重抓），
    // 跳过 TRAE 切换前 JWT 预检，否则预检 401 会把续期链路拦死
    skip_jwt_probe: Option<bool>,
) -> Result<(), String> {
    // 审查修复（入参校验/路径遍历）：user_id 会拼进豆包探测槽路径（probe_slot_session_alive）
    // 与快照槽路径，与其余 uid 入口（profile.rs/doubao.rs 等）统一过白名单
    fs_utils::ensure_uid_safe(user_id.trim())?;
    fs_utils::app_log(&state.data_dir, &format!("开始切换账号: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB（设置开关控制，其他应用不受影响）
    let is_doubao = target_app.as_deref() == Some("Doubao");
    // JWT 预检仅 TRAE 双应用（TraeWork/Trae，含默认）：WorkBuddy/CodeBuddy 会话模型不同，
    // 且其 uid 与 TRAE 账号池撞库时会被误探活错误拦截——非 trae 一律放行（CodeBuddy 同 WorkBuddy）
    let is_trae = matches!(target_app.as_deref(), None | Some("TraeWork") | Some("Trae"));
    let include_idb = is_doubao && state.settings().doubao_snapshot_include_idb;
    // 切换前服务端会话预检（仅豆包）：目标槽位快照里的会话若已被服务端吊销——常见于
    // 在豆包客户端内退出登录/重登该账号（passport logout 吊销旧会话，快照文件却完好）——
    // 恢复后客户端一联网即被 SESSION_EXPIRED 强制登出，表现为「切换成功但豆包未登录」。
    // 实测不对称现象根因：2026-09-09 A 槽探测 code=710012001（expired）、B 槽 code=0（ok）。
    // 提前拦截给出补救指引，避免白切一场；探测不可用 fail-open 不阻断（见函数内实现）。
    if is_doubao {
        crate::commands::doubao::probe_slot_session_alive(&state.data_dir, &user_id)?;
    } else if is_trae && !skip_jwt_probe.unwrap_or(false) {
        // TRAE（TraeWork/Trae）：切换前 JWT 服务端预检（issue #9）——目标账号 JWT 被服务端
        // 吊销时本地快照仍完好，切换恢复后 IDE 一联网即被登出，用户感知为「切换了但没反应」。
        // 预检 Err 仅在「判死」时产生（网络故障已在函数内 fail-open 为 Ok）。
        // 此前判死会硬拒绝切换——但签到 401 SessionDead 的账号预检必判死，而「切回该账号
        // 重新登录」正是唯一恢复手段，硬拒绝形成死结（用户反馈：签到失败的账号点切换无反应）。
        // 现改为：放行切换，把失效警示 + 恢复指引写入切换进度流（fail-open，不阻断）。
        if let Err(dead_msg) = crate::commands::accounts::probe_trae_jwt_alive(&state, &user_id) {
            fs_utils::app_log(&state.data_dir, &format!("切换前 JWT 预检判死（已放行）: {dead_msg}"));
            let warn_line = serde_json::json!({
                "stage": "probe",
                "status": "warn",
                "message": dead_msg,
            })
            .to_string();
            let _ = app.emit("switch-progress", &warn_line);
        }
    }

    // 防误覆盖守卫：把关闭客户端前检测到的当前登录 uid 传入 switcher，仅在它与
    // current_account.txt 一致时才把"当前态"回写进该账号槽。豆包走严格版（还要求
    // Live Cookies 里验证到登录会话——uid 检测可能被快照 localStorage 残留骗过，
    // Cookie 存在性无法伪造）；icube 布局（TraeWork/Trae）走本机使用证据推导（见下）
    let expected_uid = if is_doubao {
        crate::commands::doubao::detect_guard_uid_strict(&state)
    } else if is_trae {
        // icube 布局（TraeWork/Trae）切换守卫：复用 trae_apps 的本机使用证据推导
        //（apps_accounts_discover 同源实现）填充当前 uid；推导失败（None）→ 维持
        // 空串 fail-open 不阻断切换。switch_account 为 async 命令，vscdb/storage
        // 同步读取在工作线程执行，不冻结 UI
        let kind = target_app.as_deref().unwrap_or("TraeWork");
        crate::commands::trae_apps::infer_current_cloud_uid(kind).unwrap_or_default()
    } else if matches!(target_app.as_deref(), Some("WorkBuddy") | Some("CodeBuddy")) {
        // F1-3 authfile 布局（WorkBuddy/CodeBuddy）切换守卫：以共享 auth 文件当前 uid
        // 在账号池反查账号 id 填充（与 current_account.txt 同命名空间）；
        // 反查失败 → 空串 fail-open 不阻断
        crate::commands::workbuddy::pool_account_id_by_auth_uid(&state).unwrap_or_default()
    } else {
        String::new()
    };

    let args = build_args(
        Action::Switch,
        target_app.as_deref(),
        Some(user_id.clone()),
        proxy_port,
        include_idb,
        expected_uid,
        state.data_dir.clone(),
    );
    // 后台线程执行（流程含最长 ~45s 等待：优雅关闭 8s + auth 静默 10s + verify 30s，
    // 不阻塞命令返回；与原 stdout 读线程一致的异步语义）
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    let is_doubao2 = is_doubao;
    run_in_background(app2, "switch-progress", "switch-done", args, Some(Box::new(move |_app, dc_dir| {
        // 切换成功后补充该账号的账户中心（icube-dc）id 预留记录（只记录不展示）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
        if !is_doubao2 {
            let _ = crate::commands::trae_apps::backfill_dc_id_for(dc_dir, &uid_for_dc);
        }
    })));

    Ok(())
}

/// 保存当前登录态：关闭客户端 → 精准备份到 userId 槽位 → 重新启动。
/// 进度经 save-login-progress / save-login-done 事件流式返回，前端订阅契约不变。
// async：豆包分支含会话预检子进程（网络 I/O），同步命令会冻结 UI
#[tauri::command(async)]
pub fn save_current_login(
    app: AppHandle,
    state: State<AppState>,
    user_id: String,
    target_app: Option<String>,
) -> Result<(), String> {
    // 审查修复（入参校验）：同 switch_account，uid 白名单校验与其余入口对齐
    fs_utils::ensure_uid_safe(user_id.trim())?;

    // 保存前预检（仅豆包）：Live profile 必须持有登录会话 Cookie。没有 = 客户端当前未登录，
    // 保存只会把未登录状态存进账号槽（实测 908 槽被未登录态覆盖后"切换成功但永远没登录"），
    // 直接拒绝并告知补救方式。客户端此时仍在运行，Cookies 被锁由 Rust 复制到临时目录读取。
    if target_app.as_deref() == Some("Doubao") {
        crate::commands::doubao::ensure_live_has_login_session()?;
        // 服务端会话预检：本地 Cookie 存在≠会话有效。会话可能早已被服务端吊销
        // （客户端内退出过/被新登录顶替），存进去就是死会话，之后每次切换该账号都未登录
        //（实测 A 槽事故：20:43 保存的快照当时已是/随后被吊销的死会话）。expired 拒绝保存。
        crate::commands::doubao::probe_live_session_alive(&user_id)?;
    }

    fs_utils::app_log(&state.data_dir, &format!("开始保存当前登录态: user_id={user_id}"));

    // C4：豆包快照可选纳入 IndexedDB
    let include_idb = target_app.as_deref() == Some("Doubao")
        && state.settings().doubao_snapshot_include_idb;

    let args = build_args(
        Action::SaveCurrentLogin,
        target_app.as_deref(),
        Some(user_id.clone()),
        None,
        include_idb,
        String::new(),
        state.data_dir.clone(),
    );
    let is_doubao = target_app.as_deref() == Some("Doubao");
    let app2 = app.clone();
    let uid_for_dc = user_id.clone();
    run_in_background(app2, "save-login-progress", "save-login-done", args, Some(Box::new(move |_app, dc_dir| {
        // 保存登录态成功后同样补充 dc id 预留记录（快照刚生成，来源最可靠）
        // 仅 icube 布局（TraeWork/Trae）有意义；豆包快照无 storage.json，跳过
        if !is_doubao {
            let _ = crate::commands::trae_apps::backfill_dc_id_for(dc_dir, &uid_for_dc);
        }
    })));

    Ok(())
}

/// 6 层设备标识重置（switcher ResetDeviceIds 动作）
/// 进度经 device-reset-progress / device-reset-done 事件流式返回，前端订阅契约不变。
/// target_app：TraeWork（默认，TRAE SOLO CN）/ Trae（Trae CN IDE），决定清理哪个应用的数据目录
#[tauri::command(async)]
pub fn reset_device_ids(
    app: AppHandle,
    state: State<AppState>,
    target_app: Option<String>,
) -> Result<(), String> {
    let target = match target_app.as_deref() {
        Some("Trae") => "Trae",
        _ => "TraeWork",
    };
    fs_utils::app_log(&state.data_dir, "开始 6 层设备标识重置");
    let args = build_args(
        Action::ResetDeviceIds,
        Some(target),
        None,
        None,
        false,
        String::new(),
        state.data_dir.clone(),
    );
    run_in_background(app, "device-reset-progress", "device-reset-done", args, None);
    Ok(())
}
