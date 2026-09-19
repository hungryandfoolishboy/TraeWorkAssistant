//! WorkBuddy 模型目录（T2.1/F-37 静态兜底层）
//!
//! §5.6 原则：
//! - 静态兜底目录落 `wb_model_catalog.json`（缺失时以本文件内置表写入）；
//! - 能力声明永远读上游字段（inputModalities/supportedEfforts），勿硬编码——
//!   本表仅作上游目录接口（`GET {chatBase}/console/enterprises/personal/models`，
//!   批次 4 F-37 动态替换）不可用时的兜底；
//! - `effort_override` 为实测修正层（hy3 系列仅 high 真正生效，v1.2），
//!   优先级：修正层 > 客户端请求 > 上游默认。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// 单个模型的能力声明
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WbModel {
    /// 模型 id（客户端请求的 model 字段，小写）
    pub id: String,
    /// 展示名
    #[serde(default)]
    pub display: String,
    /// 上下文长度（token）
    #[serde(default)]
    pub context_length: u64,
    /// 单次最大输出
    #[serde(default)]
    pub max_tokens: u64,
    /// 是否支持图片模态（读上游 inputModalities，勿拍脑袋）
    #[serde(default)]
    pub supports_image: bool,
    /// 支持的 reasoning_effort 档位（升序）
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    /// 实测修正层：按模型族强制映射（如 "high"）
    #[serde(default)]
    pub effort_override: Option<String>,
    /// 积分倍率（展示用，成本模型按无缓存估算 §5.5 #10）
    #[serde(default)]
    pub rate: f64,
}

impl WbModel {
    /// reasoning_effort 降级：请求档位不受支持时按「≤ 请求值的最大支持档位，
    /// 否则最低档位」降级（§5.5 #3）
    pub fn resolve_effort(&self, requested: Option<&str>) -> Option<String> {
        // 修正层最优先（v1.2 §5.6）
        if let Some(o) = &self.effort_override {
            if !o.is_empty() {
                return Some(o.clone());
            }
        }
        if self.supported_efforts.is_empty() {
            return requested.map(str::to_string);
        }
        let req = match requested {
            Some(r) if !r.trim().is_empty() => r.trim().to_lowercase(),
            _ => return None, // 未请求 → 不下发，走上游默认
        };
        if self.supported_efforts.iter().any(|e| e == &req) {
            return Some(req);
        }
        let order = ["minimal", "low", "medium", "high", "xhigh", "max"];
        let rank = |e: &str| order.iter().position(|o| *o == e).unwrap_or(usize::MAX);
        let req_rank = rank(&req);
        // ≤ 请求档位的最大支持档位；都没有则最低档
        self.supported_efforts
            .iter()
            .filter(|e| rank(e) <= req_rank)
            .max_by_key(|e| rank(e))
            .or_else(|| {
                self.supported_efforts
                    .iter()
                    .min_by_key(|e| rank(e))
            })
            .cloned()
    }
}

/// 内置静态兜底目录（15 模型，§5.6；倍率为 2026-09 WorkBuddy 客户端截图快照，
/// 多界面并列时取最低：hy4-preview / hy3 限时免费 0.00x，deepseek-v4-flash 0.03x，
/// glm-5.3 / glm-5.2 0.78x，deepseek-v4-pro 0.51x，kimi-k3 / kimi-k3-1 1.62x，
/// minimax-m3 0.25x，qwen3.8-max 1.50x）
pub fn builtin() -> Vec<WbModel> {
    let m = |id: &str,
             display: &str,
             ctx: u64,
             mt: u64,
             img: bool,
             efforts: &[&str],
             ov: Option<&str>,
             rate: f64| {
        WbModel {
            id: id.to_string(),
            display: display.to_string(),
            context_length: ctx,
            max_tokens: mt,
            supports_image: img,
            supported_efforts: efforts.iter().map(|s| s.to_string()).collect(),
            effort_override: ov.map(str::to_string),
            rate,
        }
    };
    vec![
        m("hy4-preview", "Hy4 Preview", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.00),
        m("hy4", "Hy4", 1_000_000, 128_000, true, &["low", "medium", "high"], None, 0.20),
        m("hy3-x", "Hy3-X", 200_000, 64_000, false, &["high"], Some("high"), 0.05),
        m("hy3", "Hy3", 200_000, 64_000, false, &["high"], Some("high"), 0.00),
        m("glm-5.3", "GLM-5.3", 200_000, 96_000, true, &["low", "medium", "high"], None, 0.78),
        m("glm-5.3-flash", "GLM-5.3 Flash", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.06),
        m("glm-5.2", "GLM-5.2", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.78),
        m("glm-5", "GLM-5", 128_000, 64_000, false, &["low", "medium", "high"], None, 0.20),
        m("kimi-k3-1", "Kimi K3.1", 256_000, 64_000, true, &["medium", "high"], None, 1.62),
        m("kimi-k3", "Kimi K3", 256_000, 64_000, false, &["medium", "high"], None, 1.62),
        m("deepseek-v4-flash", "DeepSeek V4 Flash", 168_000, 32_000, false, &["low", "medium"], None, 0.03),
        m("deepseek-v4-pro", "DeepSeek V4 Pro", 168_000, 64_000, false, &["medium", "high"], None, 0.51),
        m("minimax-m3", "MiniMax M3", 200_000, 64_000, false, &["low", "medium", "high"], None, 0.25),
        m("qwen3.8-max", "Qwen3.8 Max", 262_000, 64_000, true, &["low", "medium", "high"], None, 1.50),
        m("qwen-3.7-plus", "Qwen 3.7 Plus", 131_000, 32_000, false, &["low", "medium", "high"], None, 0.25),
    ]
}

/// 目录文件路径（data/wb_model_catalog.json，§3.8）。
/// 数据文件统一 data/ 子目录（与 api_models/api_keys/api_usage 同层）；
/// Buddy 侧不保历史（§9.4 #7）：旧根目录位置不迁移，缺失即内置兜底重建
/// 内置表版本：倍率等快照更新时 +1。由旧版本内置表自动落盘的目录（fetched_at 为空且
/// builtin_rev 低于当前值）会按新内置表重建，使倍率修正对存量安装生效；
/// 上游同步写入的目录（fetched_at 非空）不受影响。人工维护的目录请把 builtin_rev
/// 手工改为当前值以退出重建。
pub const BUILTIN_REV: u32 = 4;

/// 目录文件结构（支持上游动态替换后的全量覆盖）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WbCatalogFile {
    #[serde(default)]
    pub models: Vec<WbModel>,
    #[serde(default)]
    pub fetched_at: Option<i64>,
    #[serde(default)]
    pub builtin_rev: u32,
}

/// 加载目录：kv `wb_model_catalog` 优先（动态替换结果 / 人工维护），
/// 缺失或为空时落盘内置表并返回内置表。
/// SQLite 化（P2）：原 data/wb_model_catalog.json → kv 键（热路径单行读取替代解析缓存）。
pub fn load(data_dir: &Path) -> Vec<WbModel> {
    // 热路径缓存（批次 A）：resolve_target / 目录聚合每请求读取（含缺失时内置表落盘副作用，
    // 该副作用在缓存未命中时执行一次即被缓存结果覆盖）
    super::config_cache::get_or_load(data_dir, "wb_model_catalog", || {
        if let Some(file) = crate::store::db(data_dir).kv_get_raw("wb_model_catalog")
            .and_then(|t| serde_json::from_str::<WbCatalogFile>(&t).ok())
        {
            if !file.models.is_empty() {
                // 上游同步结果优先保留；否则旧版本内置表落盘的目录按新内置表重建
                if file.fetched_at.is_some() || file.builtin_rev >= BUILTIN_REV {
                    return file.models;
                }
            }
        }
        let builtin = builtin();
        let _ = crate::store::db(data_dir).kv_set(
            "wb_model_catalog",
            &WbCatalogFile {
                models: builtin.clone(),
                fetched_at: None,
                builtin_rev: BUILTIN_REV,
            },
        );
        builtin
    })
}

/// 大小写不敏感查找
pub fn find<'a>(catalog: &'a [WbModel], model: &str) -> Option<&'a WbModel> {
    let lower = model.trim().to_lowercase();
    catalog.iter().find(|m| m.id == lower)
}

// ==================== T5.1/F-37 动态目录替换 ====================

/// 从单条候选对象解析模型（字段链宽容探测，能力字段读上游勿硬编码）。
fn model_from_item(item: &Value) -> Option<WbModel> {
    if !item.is_object() {
        return None;
    }
    let dig_str = |v: &Value, keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            v.get(*k).and_then(|x| x.as_str()).map(str::to_string).or_else(|| {
                v.get(*k).and_then(|x| x.as_i64()).map(|n| n.to_string())
            })
        })
    };
    let dig_u64 = |v: &Value, keys: &[&str]| -> Option<u64> {
        keys.iter().find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_u64())
                .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_f64 = |v: &Value, keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_f64())
                .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_strs = |v: &Value, keys: &[&str]| -> Vec<String> {
        keys.iter()
            .find_map(|k| {
                v.get(*k)
                    .and_then(|x| x.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .filter(|a| !a.is_empty())
            })
            .unwrap_or_default()
    };
    let Some(id) = dig_str(item, &["id", "model", "modelId", "name"])
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
    else {
        return None;
    };
    let modalities = dig_strs(item, &["inputModalities", "input_modalities", "modalities"]);
    let supports_image = modalities
        .iter()
        .any(|m| m.eq_ignore_ascii_case("image") || m.eq_ignore_ascii_case("image_url"));
    Some(WbModel {
        display: dig_str(item, &["displayName", "display", "name"]).unwrap_or_else(|| id.clone()),
        context_length: dig_u64(item, &["contextLength", "context_length", "inputTokenLimit"]).unwrap_or(0),
        max_tokens: dig_u64(item, &["maxTokens", "max_tokens", "maxOutputTokens"]).unwrap_or(0),
        supports_image,
        supported_efforts: dig_strs(item, &["supportedEfforts", "supported_efforts", "efforts"])
            .into_iter()
            .map(|e| e.to_lowercase())
            .collect(),
        effort_override: None, // 修正层仅人工/实测维护，不从上游读
        rate: dig_f64(item, &["rate", "ratio", "price", "creditRate"]).unwrap_or(0.0),
        id,
    })
}

/// 定向容器收集：从响应中找出「模型条目」候选对象列表。
/// 兼容形态（按上游演进逐版累积）：
/// 1. 根数组 / `{data: []}` / `{models: []}` / `{list: []}`（条目直接是模型）；
/// 2. **`{code:0, data:{agents:[…]}}`**（2026-09 实测新形态）——`data` 为对象，
///    模型/agent 列表挂在对象的数组字段下（agents/models/list/items/entries）；
/// 3. agent 条目内嵌 `models`/`list`/`items` 子数组（每 agent 挂自己的模型表）→ 展开子条目，
///    无内嵌子数组的 agent 条目本身作为候选（agent 即模型人格的形态）。
fn collect_candidates(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for el in arr {
                let mut nested = false;
                for key in ["models", "list", "items"] {
                    if let Some(sub) = el.get(key).and_then(Value::as_array) {
                        collect_candidates(&Value::Array(sub.clone()), out, depth + 1);
                        nested = true;
                    }
                }
                if !nested {
                    out.push(el.clone());
                }
            }
        }
        Value::Object(obj) => {
            for key in ["data", "models", "list", "items", "agents", "entries", "result"] {
                if let Some(sub) = obj.get(key) {
                    if sub.is_array() || sub.is_object() {
                        collect_candidates(sub, out, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// 深度扫描兜底（定向解析产出 0 条时）：全树递归收集「像模型条目」的对象
/// ——必须有**非空字符串**的显式 id 键（id/model/modelId/model_id）才算候选，
/// 防止把分页数字等噪音当模型。上游结构再漂移时的最后防线。
fn deep_scan_models(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for x in arr {
                deep_scan_models(x, out, depth + 1);
            }
        }
        Value::Object(_) => {
            let id_like = ["id", "model", "modelId", "model_id"].iter().any(|k| {
                matches!(v.get(*k), Some(Value::String(s)) if !s.trim().is_empty())
            });
            if id_like {
                out.push(v.clone());
            }
            if let Some(obj) = v.as_object() {
                for (_k, val) in obj {
                    deep_scan_models(val, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// 上游目录响应 → 模型列表（宽容解析：定向容器探测 → 深扫兜底 → 按 id 去重）。
/// 解析产出 0 条视为失败（不产脏目录），由调用方决定回退行为。
pub fn parse_upstream_catalog(body: &Value) -> Vec<WbModel> {
    let mut candidates: Vec<Value> = Vec::new();
    collect_candidates(body, &mut candidates, 0);
    let mut out: Vec<WbModel> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in &candidates {
        if let Some(m) = model_from_item(item) {
            if seen.insert(m.id.clone()) {
                out.push(m);
            }
        }
    }
    if out.is_empty() {
        let mut scanned: Vec<Value> = Vec::new();
        deep_scan_models(body, &mut scanned, 0);
        for item in &scanned {
            if let Some(m) = model_from_item(item) {
                if seen.insert(m.id.clone()) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// 从上游模型目录接口拉取并全量替换本地目录（T5.1/F-37 启动动态替换）。
/// 失败返回 Err（本地目录保持不动——静态兜底永远不因网络抖动被清掉）。
pub fn fetch_and_replace(
    data_dir: &Path,
    uid: &str,
    token: &str,
    domain: &str,
    enterprise_id: &str,
    global_region: bool,
) -> Result<usize, String> {
    use super::wb_upstream::{build_chat_headers, wb_agent, WbCreds};
    let creds = WbCreds {
        id: String::new(),
        uid: uid.to_string(),
        name: String::new(),
        token: token.to_string(),
        domain: domain.to_string(),
        enterprise_id: enterprise_id.to_string(),
        global_region,
    };
    let url = format!("{}/console/enterprises/personal/models", creds.chat_base());
    let mut req = wb_agent().get(&url).timeout(std::time::Duration::from_secs(20));
    for (k, v) in build_chat_headers(&creds) {
        // 目录为 GET JSON：accept 覆盖 build_chat_headers 的 text/event-stream 默认
        let v = if k.eq_ignore_ascii_case("accept") { "application/json".into() } else { v };
        req = req.set(k, &v);
    }
    let body: Value = match req.call() {
        Ok(r) => r.into_json().map_err(|e| format!("目录响应解析失败: {e}"))?,
        Err(ureq::Error::Status(code, r)) => {
            // 诊断增强：附响应体摘要，区分 401/403 鉴权拒绝与端点变更
            let text = r.into_string().unwrap_or_default();
            let excerpt: String = text.chars().take(200).collect();
            return Err(format!("目录接口 HTTP {code}: body 摘要: {excerpt}"));
        }
        Err(e) => return Err(format!("目录请求失败: {e}")),
    };
    let models = parse_upstream_catalog(&body);
    if models.is_empty() {
        // 诊断增强：附响应体摘要（截 200 字符），定位 200 状态下「结构不符」的
        // 真实形态（200 错误信封 / 多层嵌套 / 端点变更），避免只有 0 模型无从下手
        let text = serde_json::to_string(&body).unwrap_or_else(|_| format!("{body:?}"));
        let excerpt: String = text.chars().take(200).collect();
        return Err(format!(
            "上游目录解析产出 0 个模型（响应结构与预期不符，body 摘要: {excerpt}），本地目录保持不变"
        ));
    }
    let count = models.len();
    crate::store::db(data_dir)
        .kv_set(
            "wb_model_catalog",
            &WbCatalogFile {
                models,
                fetched_at: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                ),
                ..Default::default()
            },
        )
        .map_err(|e| format!("写目录失败: {e}"))?;
    // 写路径显式失效（批次 A）：目录替换即时生效
    super::config_cache::invalidate(data_dir, "wb_model_catalog");
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_15_models_and_unique_ids() {
        let c = builtin();
        assert_eq!(c.len(), 15);
        let mut ids: Vec<&str> = c.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 15);
    }

    #[test]
    fn hy3_effort_override_forces_high() {
        let c = builtin();
        let hy3 = find(&c, "hy3-x").unwrap();
        // 客户端请求 medium/xhigh 均被修正层强制为 high
        assert_eq!(hy3.resolve_effort(Some("medium")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(Some("xhigh")).as_deref(), Some("high"));
        assert_eq!(hy3.resolve_effort(None).as_deref(), Some("high"));
    }

    #[test]
    fn effort_downgrade_picks_closest_supported() {
        let c = builtin();
        // deepseek-v4-flash 仅支持 low/medium：请求 high → 降级 medium
        let ds = find(&c, "deepseek-v4-flash").unwrap();
        assert_eq!(ds.resolve_effort(Some("high")).as_deref(), Some("medium"));
        assert_eq!(ds.resolve_effort(Some("low")).as_deref(), Some("low"));
        // 请求不支持的最小档以下 → 最低档
        assert_eq!(ds.resolve_effort(Some("minimal")).as_deref(), Some("low"));
        // 未请求 → 不下发
        assert_eq!(ds.resolve_effort(None), None);
    }

    #[test]
    fn find_is_case_insensitive_and_unknown_returns_none() {
        let c = builtin();
        assert!(find(&c, "GLM-5.3").is_some());
        assert!(find(&c, "glm-5.3").is_some());
        assert!(find(&c, "not-a-model").is_none());
    }

    // ==================== T5.1/F-37 动态解析 ====================

    #[test]
    fn parse_upstream_catalog_three_container_shapes() {
        let entry = serde_json::json!({
            "id": "GLM-5.5",
            "displayName": "GLM 5.5",
            "contextLength": 200000,
            "maxTokens": 96000,
            "inputModalities": ["text", "image"],
            "supportedEfforts": ["low", "medium", "high"],
            "rate": 0.99,
        });
        let root = serde_json::json!([entry.clone()]);
        let data = serde_json::json!({"data": [entry.clone()]});
        let models = serde_json::json!({"models": [entry.clone()]});
        for body in [root, data, models] {
            let out = parse_upstream_catalog(&body);
            assert_eq!(out.len(), 1, "container shape");
            let m = &out[0];
            assert_eq!(m.id, "glm-5.5");
            assert_eq!(m.display, "GLM 5.5");
            assert_eq!(m.context_length, 200000);
            assert_eq!(m.max_tokens, 96000);
            assert!(m.supports_image, "inputModalities 含 image");
            assert_eq!(m.supported_efforts, vec!["low", "medium", "high"]);
            assert!((m.rate - 0.99).abs() < 1e-9);
            assert!(m.effort_override.is_none(), "修正层不从上游读");
        }
    }

    #[test]
    fn parse_upstream_catalog_tolerates_missing_fields_and_garbage() {
        // 缺能力字段 → 零值默认，不报错
        let out = parse_upstream_catalog(&serde_json::json!({"data": [{"id": "M1"}, {"model": "M2"}]}));
        assert_eq!(out.len(), 2);
        assert!(out[0].id == "m1" && out[1].id == "m2");
        assert!(!out[0].supports_image);
        assert!(out[0].supported_efforts.is_empty());
        // 非 dict 条目 / 缺 id → 跳过
        let out = parse_upstream_catalog(&serde_json::json!({"data": ["junk", {"displayName": "无id"}, 42]}));
        assert!(out.is_empty());
        // 完全无关结构 → 空列表（调用方保持本地目录不变）
        assert!(parse_upstream_catalog(&serde_json::json!({"foo": 1})).is_empty());
        assert!(parse_upstream_catalog(&serde_json::json!("text")).is_empty());
    }

    #[test]
    fn parse_upstream_catalog_numeric_and_string_fields() {
        let body = serde_json::json!({"data": [
            {"id": "hy5", "contextLength": "1000000", "rate": "0.35", "modalities": ["image_url"]},
        ]});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].context_length, 1_000_000, "字符串数值宽容解析");
        assert!((out[0].rate - 0.35).abs() < 1e-9);
        assert!(out[0].supports_image, "modalities 别名键");
    }

    #[test]
    fn parse_upstream_catalog_data_object_with_agents() {
        // 2026-09 实测新形态：data 为对象，agents[] 挂在对象字段下（每 agent 内嵌 models）
        let entry = serde_json::json!({
            "id": "glm-5.3",
            "displayName": "GLM 5.3",
            "contextLength": 200000,
            "inputModalities": ["text", "image"],
        });
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "agents": [
                    {"commands": ["init", "compact", "statusline", "insights"], "models": [entry.clone()]},
                    {"commands": ["init"], "models": [{"id": "hy4-preview", "displayName": "Hy4 Preview"}]},
                ]
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 2, "agents 内嵌 models 展开");
        assert_eq!(out[0].id, "glm-5.3");
        assert!(out[0].supports_image);
        assert_eq!(out[1].id, "hy4-preview");
    }

    #[test]
    fn parse_upstream_catalog_agents_as_model_entries() {
        // agent 条目无内嵌 models 子数组 → 条目本身作为模型候选（agent 即模型人格形态）
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "agents": [
                    {"id": "kimi-k3", "name": "Kimi K3", "commands": ["init", "compact"]},
                    {"id": "deepseek-v4-pro", "name": "DeepSeek V4 Pro", "commands": ["statusline"]},
                ]
            }
        });
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "kimi-k3");
        assert_eq!(out[0].display, "Kimi K3");
        assert_eq!(out[1].id, "deepseek-v4-pro");
    }

    #[test]
    fn parse_upstream_catalog_deep_scan_fallback_and_dedupe() {
        // 定向容器全不命中 → 全树深扫兜底（要求非空字符串显式 id）
        let body = serde_json::json!({"x": {"y": [{"meta": 1}, {"model": "unknown-model"}]}});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "unknown-model");
        // 同 id 去重（保留首个）
        let body = serde_json::json!({"data": [{"id": "M1"}, {"id": "M1"}, {"id": "m1"}]});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1, "大小写不敏感去重");
        // 数字 id / 空 id / 纯噪音不进深扫结果
        let body = serde_json::json!({"a": {"id": 3}, "b": {"id": ""}, "c": {"id": "ok-model"}});
        let out = parse_upstream_catalog(&body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ok-model");
    }
}
