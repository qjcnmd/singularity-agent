//! 模型限额目录：编译期内置表 + `metadata-cache.json` 投影缓存。
//!
//! 限额解析优先级为用户配置声明 > 内置表 > 目录缓存 > 保守默认（不猜测）。
//! 缓存读路径只读本地文件、永不联网；坏条目单条剔除，整体损坏视为无缓存。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::user::user_config_directory_result;
use crate::{DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS};

/// 目录投影缓存文件的生命周期（与决策记录 D-045 的 TTL 一致）。
pub(crate) const METADATA_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// `metadata-cache.json` 的文件名。
pub(crate) const METADATA_CACHE_FILE_NAME: &str = "metadata-cache.json";
/// 单次读取缓存文件的字节上限（有界读取，超限视为无缓存）。
const MAX_CACHE_FILE_BYTES: usize = 8 * 1024 * 1024;

/// 一个模型的窗口限额。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelLimits {
    pub(crate) context: u32,
    pub(crate) output: u32,
}

/// 目录投影缓存的磁盘格式（version 1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedCatalog {
    pub(crate) version: u32,
    /// 缓存抓取时间（RFC 3339）；超过 TTL 的缓存视为不存在。
    #[serde(rename = "fetchedAt")]
    pub(crate) fetched_at: String,
    pub(crate) providers: BTreeMap<String, CachedProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedProvider {
    pub(crate) models: BTreeMap<String, CachedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedModel {
    pub(crate) context: u32,
    pub(crate) output: u32,
}

/// 三级回退的解析结果；未知模型保持保守默认（不猜测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitsFallback {
    Builtin,
    Cache,
    Conservative,
}

impl CachedCatalog {
    /// 从默认 home 目录读取投影缓存；缺失、损坏、过期或超限一律视为无缓存。
    pub(crate) fn load_default() -> Option<CachedCatalog> {
        let directory = user_config_directory_result().ok()??;
        let path = directory.join(METADATA_CACHE_FILE_NAME);
        let catalog = load_catalog_file(&path)?;
        catalog.is_fresh().then_some(catalog)
    }

    /// 缓存是否仍在 TTL 内（fetchedAt 解析失败视为不新鲜）。
    fn is_fresh(&self) -> bool {
        let Ok(fetched_at) = time::OffsetDateTime::parse(
            &self.fetched_at,
            &time::format_description::well_known::Rfc3339,
        ) else {
            return false;
        };
        let age = time::OffsetDateTime::now_utc() - fetched_at;
        age < METADATA_CACHE_TTL
    }

    /// 在缓存中查询模型限额；model id 精确匹配 + 大小写回退。
    fn lookup(&self, provider: &str, model: &str) -> Option<ModelLimits> {
        let cached_provider = self.providers.get(provider)?;
        if let Some(cached) = cached_provider.models.get(model) {
            return Some(ModelLimits {
                context: cached.context,
                output: cached.output,
            });
        }
        cached_provider
            .models
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(model))
            .map(|(_, cached)| ModelLimits {
                context: cached.context,
                output: cached.output,
            })
    }
}

/// 从磁盘解析缓存文件；任何结构级缺陷（非 JSON、缺 version、超限）返回 None。
fn load_catalog_file(path: &Path) -> Option<CachedCatalog> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_CACHE_FILE_BYTES as u64 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parse_catalog_payload(&parsed)
}

/// 解析缓存载荷：结构级缺陷 fail 为 None；单个坏 provider/model 条目被跳过。
fn parse_catalog_payload(payload: &serde_json::Value) -> Option<CachedCatalog> {
    let object = payload.as_object()?;
    let version = object.get("version").and_then(serde_json::Value::as_u64)?;
    if version != 1 {
        return None;
    }
    let fetched_at = object.get("fetchedAt")?.as_str()?.to_string();
    let mut providers = BTreeMap::new();
    for (provider_name, provider_value) in object.get("providers")?.as_object()? {
        let Some(models_value) = provider_value.get("models").and_then(|v| v.as_object()) else {
            continue;
        };
        let mut models = BTreeMap::new();
        for (model_id, model_value) in models_value {
            let Some(context) = model_value.get("context").and_then(|v| v.as_u64()) else {
                continue;
            };
            let Some(output) = model_value.get("output").and_then(|v| v.as_u64()) else {
                continue;
            };
            let Ok(context) = u32::try_from(context) else {
                continue;
            };
            let Ok(output) = u32::try_from(output) else {
                continue;
            };
            models.insert(model_id.clone(), CachedModel { context, output });
        }
        if !models.is_empty() {
            providers.insert(provider_name.clone(), CachedProvider { models });
        }
    }
    Some(CachedCatalog {
        version: 1,
        fetched_at,
        providers,
    })
}

/// 解析模型限额：用户配置缺省时按 内置表 → 目录缓存 → 保守默认 回退。
pub(crate) fn resolve_model_limits(
    provider: &str,
    model: &str,
    cache: Option<&CachedCatalog>,
) -> (u32, u32, LimitsFallback) {
    if let Some(limits) = builtin_limits(provider, model) {
        return (limits.context, limits.output, LimitsFallback::Builtin);
    }
    if let Some(limits) = cache.and_then(|cache| cache.lookup(provider, model)) {
        return (limits.context, limits.output, LimitsFallback::Cache);
    }
    (
        DEFAULT_MAX_CONTEXT_TOKENS,
        DEFAULT_MAX_OUTPUT_TOKENS,
        LimitsFallback::Conservative,
    )
}

macro_rules! limits_table {
    ($($model:literal => $context:literal, $output:literal;)*) => {
        {
            let mut table = BTreeMap::new();
            $(table.insert($model.to_string(), ModelLimits { context: $context, output: $output });)*
            table
        }
    };
}

fn builtin_table_deepseek() -> BTreeMap<String, ModelLimits> {
    limits_table! {
        "deepseek-v4-flash" => 1_000_000, 384_000;
        "deepseek-v4-flash-0731" => 1_000_000, 384_000;
        "deepseek-v4-pro" => 1_000_000, 384_000;
        "deepseek-chat" => 1_000_000, 384_000;
        "deepseek-reasoner" => 1_000_000, 384_000;
    }
}

fn builtin_table_openai() -> BTreeMap<String, ModelLimits> {
    limits_table! {
        "gpt-5" => 400_000, 128_000;
        "gpt-5-mini" => 400_000, 128_000;
        "gpt-5-nano" => 400_000, 128_000;
        "gpt-5-pro" => 400_000, 272_000;
        "gpt-4.1" => 1_047_576, 32_768;
        "gpt-4.1-mini" => 1_047_576, 32_768;
        "gpt-4o" => 128_000, 16_384;
        "gpt-4o-mini" => 128_000, 16_384;
        "o3" => 200_000, 100_000;
        "o3-mini" => 200_000, 100_000;
        "o4-mini" => 200_000, 100_000;
    }
}

fn builtin_table_anthropic() -> BTreeMap<String, ModelLimits> {
    limits_table! {
        "claude-opus-4-5" => 200_000, 64_000;
        "claude-opus-4-6" => 1_000_000, 128_000;
        "claude-sonnet-4-5" => 1_000_000, 64_000;
        "claude-sonnet-4-6" => 1_000_000, 128_000;
        "claude-haiku-4-5" => 200_000, 64_000;
    }
}

/// 内置表的懒加载单例（仅首次查询构建一次）。
fn builtin_table(provider: &str) -> Option<&'static BTreeMap<String, ModelLimits>> {
    use std::sync::OnceLock;
    static DEEPSEEK: OnceLock<BTreeMap<String, ModelLimits>> = OnceLock::new();
    static OPENAI: OnceLock<BTreeMap<String, ModelLimits>> = OnceLock::new();
    static ANTHROPIC: OnceLock<BTreeMap<String, ModelLimits>> = OnceLock::new();
    match provider {
        "deepseek" => Some(DEEPSEEK.get_or_init(builtin_table_deepseek)),
        "openai" | "openai_compatible" => Some(OPENAI.get_or_init(builtin_table_openai)),
        "anthropic" => Some(ANTHROPIC.get_or_init(builtin_table_anthropic)),
        _ => None,
    }
}

fn builtin_limits(provider: &str, model: &str) -> Option<ModelLimits> {
    let table = builtin_table(provider)?;
    if let Some(limits) = table.get(model) {
        return Some(*limits);
    }
    table
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model))
        .map(|(_, limits)| *limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_payload(fetched_at: &str) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "fetchedAt": fetched_at,
            "providers": {
                "catalog-provider": {
                    "models": {
                        "catalog-model": {"context": 99_000, "output": 8_000},
                        "catalog-model-upper": {"context": 77_000, "output": 7_000}
                    }
                }
            }
        })
    }

    #[test]
    fn builtin_table_exact_and_case_insensitive_match() {
        let limits = builtin_limits("deepseek", "deepseek-v4-flash").expect("builtin deepseek");
        assert_eq!(limits.context, 1_000_000);
        assert_eq!(limits.output, 384_000);
        let upper = builtin_limits("deepseek", "DEEPSEEK-V4-FLASH").expect("case fallback");
        assert_eq!(upper, limits);
        assert!(builtin_limits("unknown-provider", "any").is_none());
    }

    #[test]
    fn catalog_lookup_exact_and_case_insensitive_match() {
        let now = time::OffsetDateTime::now_utc();
        let fetched = now
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format");
        let catalog = parse_catalog_payload(&cache_payload(&fetched)).expect("parse");
        let exact = catalog
            .lookup("catalog-provider", "catalog-model")
            .expect("exact match");
        assert_eq!(exact.context, 99_000);
        let upper = catalog
            .lookup("catalog-provider", "CATALOG-MODEL")
            .expect("case fallback");
        assert_eq!(upper, exact);
        assert!(
            catalog
                .lookup("missing-provider", "catalog-model")
                .is_none()
        );
        assert!(
            catalog
                .lookup("catalog-provider", "missing-model")
                .is_none()
        );
    }

    #[test]
    fn stale_catalog_is_treated_as_absent() {
        let stale = cache_payload("2020-01-01T00:00:00Z");
        let catalog = parse_catalog_payload(&stale).expect("parse");
        assert!(!catalog.is_fresh(), "catalog older than TTL must be stale");
    }

    #[test]
    fn malformed_catalog_entries_are_skipped_and_structure_failures_fail_soft() {
        let payload = serde_json::json!({
            "version": 1,
            "fetchedAt": "2026-01-01T00:00:00Z",
            "providers": {
                "good": {
                    "models": {
                        "ok-model": {"context": 10_000, "output": 2_000},
                        "bad-context": {"context": "x", "output": 2_000},
                        "bad-output": {"context": 10_000},
                        "huge": {"context": 9_000_000_000u64, "output": 2_000}
                    }
                },
                "broken-provider": "not-an-object"
            }
        });
        let catalog = parse_catalog_payload(&payload).expect("parse with skipped entries");
        let ok = catalog
            .lookup("good", "ok-model")
            .expect("valid entry kept");
        assert_eq!(ok.context, 10_000);
        assert!(catalog.lookup("good", "bad-context").is_none());
        assert!(catalog.lookup("good", "huge").is_none());
        assert!(catalog.lookup("broken-provider", "any").is_none());

        assert!(parse_catalog_payload(&serde_json::json!({"version": 2})).is_none());
        assert!(parse_catalog_payload(&serde_json::json!("nope")).is_none());
    }

    #[test]
    fn resolution_prefers_builtin_then_cache_then_conservative() {
        let now = time::OffsetDateTime::now_utc();
        let fetched = now
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format");
        let catalog = parse_catalog_payload(&cache_payload(&fetched)).expect("parse");

        let (context, output, fallback) =
            resolve_model_limits("deepseek", "deepseek-v4-flash", Some(&catalog));
        assert_eq!(fallback, LimitsFallback::Builtin);
        assert_eq!(context, 1_000_000);

        let (context, output, fallback) =
            resolve_model_limits("catalog-provider", "catalog-model", Some(&catalog));
        assert_eq!(fallback, LimitsFallback::Cache);
        assert_eq!(context, 99_000);
        assert_eq!(output, 8_000);

        let (context, output, fallback) =
            resolve_model_limits("catalog-provider", "unknown-model", Some(&catalog));
        assert_eq!(fallback, LimitsFallback::Conservative);
        assert_eq!(context, DEFAULT_MAX_CONTEXT_TOKENS);
        assert_eq!(output, DEFAULT_MAX_OUTPUT_TOKENS);
    }
}
