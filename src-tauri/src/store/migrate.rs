//! 启动迁移器：旧 data/ JSON 全量导入 SQLite，成功后原文件移入 `data/backup/`（幂等）。
//!
//! 三态（docs/sqllite-storage-plan.md §三）：
//! - 正常：解析 → 写库 → `fs::rename` 到 `backup/<相对路径>`；
//! - 损坏：JSON 解析失败 → 移 `backup/corrupt/`（对齐 read_json「坏文件回退默认」）；
//! - 失败：IO/DB 错误 → 保留原位，下次启动重试（不置 user_version）。
//!
//! 全部条目处理完且零失败才置 `PRAGMA user_version = 1`；明细落
//! `data/backup/migration_manifest.json`。失败不阻断启动（返回摘要由调用方写日志）。

use serde_json::{json, Value};
use std::path::Path;

use super::{db, docs, schema, Store};

/// KV 组：（相对路径 → kv 键）。content = 文件文本原样（校验为合法 JSON 后入库）。
const KV_ENTRIES: &[(&str, &str)] = &[
    ("conf/app_settings.json", "app_settings"),
    ("data/api_pool.json", "api_pool"),
    ("data/dispatch_policy.json", "dispatch_policy"),
    ("data/api_gateway_settings.json", "api_gateway_settings"),
    ("data/api_models.json", "api_models"),
    ("data/wb_model_catalog.json", "wb_model_catalog"),
    ("data/trae_model_meta.json", "trae_model_meta"),
    ("data/wb_model_route.json", "wb_model_route"),
    ("data/wb_template_map.json", "wb_template_map"),
    ("data/wb_sticky_sessions.json", "wb_sticky_sessions"),
    ("data/checkin_summary.json", "checkin_summary"),
    ("data/workbuddy_settings.json", "workbuddy_settings"),
    ("data/workbuddy_credits_history.json", "workbuddy_credits_history"),
    ("data/workbuddy_credits_cache.json", "workbuddy_credits_cache"),
    ("data/workbuddy_usage_official_cache.json", "workbuddy_usage_official_cache"),
    ("data/workbuddy_usage_official_all_cache.json", "workbuddy_usage_official_all_cache"),
    ("data/workbuddy_activity_cache.json", "workbuddy_activity_cache"),
    ("data/wb_cli_rotate_state.json", "wb_cli_rotate_state"),
    ("data/token_stats_files.json", "token_stats_files"),
    ("data/doubao_renew_result.json", "doubao_renew_result"),
    ("data/usage_history.json", "usage_history"),
    ("data/oauth_device.json", "oauth_device"),
    ("data/scheduler_state.json", "scheduler_state"),
    ("data/doubao_captured_credentials.json", "doubao_captured_credentials"),
];

/// 结构化组：（相对路径 → 导入函数）
const DOC_ENTRIES: &[(&str, fn(&Store, &Path) -> ImportStatus)] = &[
    ("data/checkin_accounts.json", import_accounts),
    ("data/device_map.json", import_device_map),
    ("data/groups.json", import_groups),
    ("data/remaining_credits.json", import_remaining_credits),
    ("data/account_cooldowns.json", import_account_cooldowns),
    ("data/pay_status.json", import_pay_status),
    ("data/api_keys.json", import_api_keys),
    ("data/custom_models.json", import_custom_models),
    ("data/doubao_accounts.json", import_doubao_pool),
    ("data/workbuddy_accounts.json", import_wb_pool),
    ("data/workbuddy_token_store.json", import_wb_token_store),
    ("data/api_usage.json", import_api_usage),
    ("data/credits_history.json", import_credits_history),
    ("data/credits_daily.json", import_credits_daily),
    ("data/checkin_results.json", import_checkin_results),
    ("data/workbuddy_checkin_results.json", import_wb_checkin_results),
    ("data/doubao_health_history.json", import_doubao_health),
];

/// 遗留根路径兜底（存在才迁移；含 wb_upstream 死引用曾写出的根路径 token store）。
/// （相对路径, 目标 kv 键 或 None=走结构化导入器）
const LEGACY_ROOT: &[(&str, Option<&str>)] = &[
    ("workbuddy_token_store.json", None),
    ("wb_template_map.json", Some("wb_template_map")),
    ("api_models.json", Some("api_models")),
    ("wb_sticky_sessions.json", Some("wb_sticky_sessions")),
    ("wb_model_route.json", Some("wb_model_route")),
    ("checkin_accounts.json", None),
];

enum ImportStatus {
    Ok,
    Corrupt(String),
    Error(String),
}

/// 迁移入口。返回 None = 无需迁移（库已是最新）；Some = 摘要（调用方写 app_log）。
pub fn migrate_on_startup(data_dir: &Path) -> Option<String> {
    let store = db(data_dir);
    let current = store
        .with_conn(|c| Ok(schema::user_version(c)))
        .unwrap_or(0);
    if current >= schema::SCHEMA_VERSION {
        return None;
    }

    let mut items: Vec<Value> = Vec::new();
    let mut failed = 0usize;
    let mut imported = 0usize;
    let mut corrupt = 0usize;
    let mut missing = 0usize;
    // 已成功导入的正牌文件集合（遗留根路径兜底仅在其正牌文件缺失时执行，
    // 防止根路径残留覆盖 data/ 正牌数据）
    let mut imported_rels: std::collections::HashSet<String> = Default::default();

    for (rel, key) in KV_ENTRIES {
        match import_kv(&store, data_dir, rel, key) {
            ItemResult::Imported => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                items.push(json!({"file": rel, "result": "imported", "target": format!("kv:{key}")}));
                move_to_backup(data_dir, rel, false);
            }
            ItemResult::Missing => {
                missing += 1;
                items.push(json!({"file": rel, "result": "missing"}));
            }
            ItemResult::Corrupt(e) => {
                corrupt += 1;
                items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                move_to_backup(data_dir, rel, true);
            }
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (rel, importer) in DOC_ENTRIES {
        let path = data_dir.join(rel);
        if !path.exists() {
            missing += 1;
            items.push(json!({"file": rel, "result": "missing"}));
            continue;
        }
        match importer(&store, &path) {
            ImportStatus::Ok => {
                imported += 1;
                imported_rels.insert(rel.to_string());
                items.push(json!({"file": rel, "result": "imported"}));
                move_to_backup(data_dir, rel, false);
            }
            ImportStatus::Corrupt(e) => {
                corrupt += 1;
                items.push(json!({"file": rel, "result": "corrupt", "detail": e}));
                move_to_backup(data_dir, rel, true);
            }
            ImportStatus::Error(e) => {
                failed += 1;
                items.push(json!({"file": rel, "result": "error", "detail": e}));
            }
        }
    }

    for (name, kv_key) in LEGACY_ROOT {
        let path = data_dir.join(name);
        if !path.exists() {
            continue;
        }
        // 正牌文件（data/ 下同名）已导入 → 根路径残留仅作垃圾清理
        if imported_rels.contains(&format!("data/{name}")) {
            items.push(json!({"file": name, "result": "skipped_stale", "scope": "legacy_root"}));
            move_legacy_root_to_backup(data_dir, name);
            continue;
        }
        let result = match kv_key {
            Some(key) => import_kv(&store, data_dir, name, key),
            None => {
                let importer = if *name == "checkin_accounts.json" { import_accounts } else { import_wb_token_store };
                to_item(run_importer(&store, &path, importer))
            }
        };
        match result {
            ItemResult::Imported => {
                imported += 1;
                items.push(json!({"file": name, "result": "imported", "scope": "legacy_root"}));
                move_legacy_root_to_backup(data_dir, name);
            }
            ItemResult::Corrupt(_) => {
                corrupt += 1;
                items.push(json!({"file": name, "result": "corrupt", "scope": "legacy_root"}));
                move_legacy_root_to_backup(data_dir, name);
            }
            ItemResult::Missing => {}
            ItemResult::Error(e) => {
                failed += 1;
                items.push(json!({"file": name, "result": "error", "detail": e}));
            }
        }
    }

    // 全部条目处理完且零失败才置版本号（有失败保留原位，下次启动重试）
    if failed == 0 {
        let _ = store.with_conn(|c| {
            schema::set_user_version(c, schema::SCHEMA_VERSION);
            Ok(())
        });
    }

    // 明细清单落 backup/
    let manifest = json!({
        "started_at": crate::fs_utils::now_ts(),
        "schema_version": schema::SCHEMA_VERSION,
        "imported": imported,
        "missing": missing,
        "corrupt": corrupt,
        "failed": failed,
        "items": items,
    });
    let backup_dir = data_dir.join("data").join("backup");
    let _ = std::fs::create_dir_all(&backup_dir);
    let _ = crate::fs_utils::write_json(&backup_dir.join("migration_manifest.json"), &manifest);

    Some(format!(
        "SQLite 迁移完成：导入 {imported}，缺失 {missing}，损坏 {corrupt}，失败 {failed}（明细见 data/backup/migration_manifest.json）"
    ))
}

enum ItemResult {
    Imported,
    Missing,
    Corrupt(String),
    Error(String),
}

/// KV 导入：文件文本校验为合法 JSON 后原样入库
fn import_kv(store: &Store, data_dir: &Path, rel: &str, key: &str) -> ItemResult {
    let path = data_dir.join(rel);
    if !path.exists() {
        return ItemResult::Missing;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return ItemResult::Corrupt("空文件".into());
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => match store.kv_set_raw(key, &v.to_string()) {
                    Ok(()) => ItemResult::Imported,
                    Err(e) => ItemResult::Error(e),
                },
                Err(e) => ItemResult::Corrupt(format!("JSON 解析失败: {e}")),
            }
        }
        Err(e) => ItemResult::Error(format!("读取失败: {e}")),
    }
}

fn run_importer(store: &Store, path: &Path, f: fn(&Store, &Path) -> ImportStatus) -> ImportStatus {
    f(store, path)
}

fn to_item(s: ImportStatus) -> ItemResult {
    match s {
        ImportStatus::Ok => ItemResult::Imported,
        ImportStatus::Corrupt(e) => ItemResult::Corrupt(e),
        ImportStatus::Error(e) => ItemResult::Error(e),
    }
}

/// 原文件移入 backup/（corrupt=true 时入 backup/corrupt/，按文件名平铺）
fn move_to_backup(data_dir: &Path, rel: &str, corrupt: bool) {
    let src = data_dir.join(rel);
    let file_name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.to_string());
    let backup_dir = data_dir
        .join("data")
        .join("backup")
        .join(if corrupt { "corrupt" } else { "" });
    let _ = std::fs::create_dir_all(&backup_dir);
    let dst = backup_dir.join(&file_name);
    if dst.exists() {
        // 同名冲突：加时间戳后缀，绝不覆盖既有备份
        let stamped = format!(
            "{}_{}",
            file_name.trim_end_matches(".json"),
            chrono::Local::now().format("%Y%m%d%H%M%S")
        );
        let _ = std::fs::rename(&src, backup_dir.join(format!("{stamped}.json")));
        return;
    }
    let _ = std::fs::rename(&src, dst);
}

/// 遗留根路径文件 → backup/legacy_root/
fn move_legacy_root_to_backup(data_dir: &Path, name: &str) {
    let src = data_dir.join(name);
    let dir = data_dir.join("data").join("backup").join("legacy_root");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::rename(&src, dir.join(name));
}

// ── 结构化导入器（读原 struct → docs::save；解析失败 = Corrupt）──────────────

fn import_accounts(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::AccountsFile>(&text) {
            Ok(f) => wrap(docs::accounts_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_device_map(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let map: crate::models::DeviceMap = serde_json::from_value(v).map_err(ser_err)?;
        docs::device_map_save(store, &map)
    })
}

fn import_groups(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::GroupsFile>(&text) {
            Ok(f) => wrap(docs::groups_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_remaining_credits(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::RemainingCreditsFile>(&text) {
            Ok(f) => wrap(docs::remaining_credits_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_account_cooldowns(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::AccountCooldownsFile>(&text) {
            Ok(f) => wrap(docs::account_cooldowns_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_pay_status(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let f: crate::commands::trae_apps::PayStatusFile =
            serde_json::from_value(v).map_err(ser_err)?;
        docs::pay_status_save(store, &f)
    })
}

fn import_api_keys(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::api_keys::ApiKeysFile>(&text) {
            Ok(f) => wrap(docs::api_keys_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_custom_models(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::custom_models::CustomModelsFile>(&text) {
            Ok(f) => wrap(docs::custom_models_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_doubao_pool(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            match serde_json::from_str::<crate::commands::doubao::DoubaoAccountPool>(&text) {
                Ok(pool) => wrap(docs::doubao_pool_save(store, &pool)),
                // DoubaoAccountPool 序列化含 private 字段默认值兜底，解析失败按损坏处理
                Err(e) => ImportStatus::Corrupt(e.to_string()),
            }
        }
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_pool(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_pool_save(store, &v))
}

fn import_wb_token_store(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_token_store_save(store, &v))
}

fn import_api_usage(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::api_server::usage::UsageFile>(&text) {
            Ok(f) => wrap(docs::api_usage_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_history(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsFile>(&text) {
            Ok(f) => wrap(docs::credits_history_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_credits_daily(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::models::CreditsDailyFile>(&text) {
            Ok(f) => wrap(docs::credits_daily_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<crate::checkin_results::ResultsFile>(&text) {
            Ok(f) => wrap(docs::checkin_results_save(store, &f)),
            Err(e) => ImportStatus::Corrupt(e.to_string()),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

fn import_wb_checkin_results(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| docs::wb_checkin_results_save(store, &v))
}

fn import_doubao_health(store: &Store, path: &Path) -> ImportStatus {
    parse_then(path, |v| {
        let empty = Vec::new();
        let events = v.get("events").and_then(Value::as_array).unwrap_or(&empty);
        docs::doubao_health_save(store, events)
    })
}

// ── 工具 ─────────────────────────────────────────────────────────────────────

fn parse_then(
    path: &Path,
    f: impl FnOnce(Value) -> Result<(), String>,
) -> ImportStatus {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => match f(v) {
                Ok(()) => ImportStatus::Ok,
                Err(e) => ImportStatus::Error(e),
            },
            Err(e) => ImportStatus::Corrupt(format!("JSON 解析失败: {e}")),
        },
        Err(e) => ImportStatus::Error(e.to_string()),
    }
}

/// pay_status 等 struct 导入的 serde 错误转换（parse_then 闭包用）
fn ser_err(e: serde_json::Error) -> String {
    e.to_string()
}

fn wrap(r: Result<(), String>) -> ImportStatus {
    match r {
        Ok(()) => ImportStatus::Ok,
        Err(e) => ImportStatus::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("twa_migrate_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("conf")).unwrap();
        std::fs::create_dir_all(d.join("data")).unwrap();
        d
    }

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn migrate_imports_moves_and_is_idempotent() {
        let dir = tmp_dir("full");
        // 造三类数据：KV、行文档、列化流水 + 一个损坏文件
        write(&dir.join("conf/app_settings.json"), r#"{"theme":"dark","proxy_port":8899}"#);
        write(&dir.join("data/api_pool.json"), r#"{"enabled_uids":["u1"],"strategy":"expire_first"}"#);
        write(
            &dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"a1","UserID":"u1","jwt":"j1"},{"name":"a2","jwt":""}]}"#,
        );
        write(
            &dir.join("data/credits_history.json"),
            r#"{"records":[{"date":"2026-09-15","user_id":"u1","credits":100,"delta":10}]}"#,
        );
        write(&dir.join("data/device_map.json"), r#"{"u1":{"device_id":"924145245134852"}}"#);
        write(&dir.join("data/pay_status.json"), "not-json");
        write(&dir.join("data/workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a1":{"access_token":"t"}}}"#);
        // 遗留根路径兜底
        write(&dir.join("workbuddy_token_store.json"), r#"{"version":1,"tokens":{"a2":{"access_token":"t2"}}}"#);

        let summary = migrate_on_startup(&dir).expect("首次应触发迁移");
        assert!(summary.contains("导入"), "摘要应含导入数: {summary}");
        assert!(summary.contains("损坏"), "摘要应含损坏计数: {summary}");
        assert!(!summary.contains("失败 1"), "不应有失败项: {summary}");

        // 库生成且版本置位
        let store = db(&dir);
        let v = store.with_conn(|c| Ok(schema::user_version(c))).unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);

        // 原文件移入 backup/
        assert!(dir.join("data/backup/conf").join("app_settings.json").exists() || dir.join("data/backup/app_settings.json").exists()
            || dir.join("data/backup").join("app_settings.json").exists());
        assert!(dir.join("data/backup/legacy_root/workbuddy_token_store.json").exists());
        assert!(dir.join("data/backup/corrupt/pay_status.json").exists());
        assert!(!dir.join("data/checkin_accounts.json").exists());
        // manifest 落盘
        assert!(dir.join("data/backup/migration_manifest.json").exists());

        // 数据验证：accounts 两行、credits_history 一条、kv 生效、wb_tokens 两行（data + legacy）
        let acc = docs::accounts_load(&store);
        assert_eq!(acc.accounts.len(), 2);
        assert_eq!(acc.accounts[0].user_id.as_deref(), Some("u1"));
        assert_eq!(acc.accounts[0].jwt, "j1");
        assert_eq!(acc.accounts[1].user_id, None);
        let ch = docs::credits_history_load(&store);
        assert_eq!(ch.records.len(), 1);
        assert_eq!(ch.records[0].credits, 100);
        let settings: Value = store.kv_get("app_settings");
        assert_eq!(settings["theme"], "dark");
        let tokens = docs::wb_token_store_load(&store);
        assert_eq!(tokens["tokens"]["a1"]["access_token"], "t");
        // 遗留根路径残留不覆盖正牌数据（仅清理，a2 不得混入）
        assert!(tokens["tokens"].get("a2").is_none());

        // 幂等：二次迁移不执行
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_dir_first_boot_creates_empty_db() {
        let dir = tmp_dir("empty");
        let summary = migrate_on_startup(&dir).expect("空目录也应完成建库");
        assert!(summary.contains("导入 0"));
        assert!(dir.join("data/aiwork.sqlite").exists());
        assert!(migrate_on_startup(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
