//! models.dev 目录限额元数据：独立缓存、投影解析与捕获时自动填充。
//!
//! 读路径（配置捕获）只读本地缓存文件，永不联网；缓存缺失、过期或损坏时不
//! 填充，能力解析维持 fail closed。写路径在既有模型目录发现刷新成功后顺带
//! 拉取 `https://models.dev/api.json` 并落盘，网络失败 fail-soft 不影响主流程。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::config::filesystem::{read_bounded_text, write_json_file};
use crate::config::schema::{deserialize_unique_map, deserialize_unique_vec};
use crate::config::{
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS, configuration_error,
    validate_model_id, validate_provider_identifier,
};
use crate::transport::http::{
    block_on_provider_future, model_error_from_http_status, read_bounded_provider_response_body,
};
use crate::{
    MAX_DISCOVERED_MODEL_IDS, MAX_DISCOVERY_RESPONSE_BYTES, METADATA_CACHE_SCHEMA_VERSION,
    METADATA_DIRECTORY_URL, PROVIDER_TIMEOUT_SECONDS, USER_MODELS_CACHE_TTL_SECONDS,
};

/// models.dev 投影出的单模型 token 限额子集。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelTokenLimits {
    pub(crate) context: u32,
    pub(crate) output: u32,
}

/// 缓存中一个目录 provider 的投影条目。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetadataCacheProvider {
    /// 该 provider 官方 API 端点的小写 host（来自目录 `api` 字段），
    /// 供用户 provider 名不匹配时的 base_url host 次级映射使用。
    #[serde(default, deserialize_with = "deserialize_unique_vec")]
    pub(crate) hosts: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) models: BTreeMap<String, ModelTokenLimits>,
}

/// `metadata-cache.json` 的磁盘 schema：仅存投影所需字段以控体积。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MetadataCacheFile {
    pub(crate) schema_version: u32,
    pub(crate) fetched_at_unix_seconds: u64,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) providers: BTreeMap<String, MetadataCacheProvider>,
}

/// 捕获读路径使用的内存投影：原始目录 key + 唯一 host 反查表。
pub(crate) struct ModelMetadataDirectory {
    providers: BTreeMap<String, BTreeMap<String, ModelTokenLimits>>,
    hosts: BTreeMap<String, String>,
}

impl ModelMetadataDirectory {
    fn empty() -> Self {
        Self {
            providers: BTreeMap::new(),
            hosts: BTreeMap::new(),
        }
    }

    fn from_cache(cache: MetadataCacheFile) -> Self {
        let mut directory = Self::empty();
        let mut ambiguous = BTreeSet::new();
        for (provider_key, entry) in cache.providers {
            for host in entry.hosts {
                if ambiguous.contains(&host) {
                    continue;
                }
                match directory.hosts.get(&host) {
                    // 同一 host 对应多个目录条目时无法唯一归因，该 host 弃用。
                    Some(existing) if existing != &provider_key => {
                        directory.hosts.remove(&host);
                        ambiguous.insert(host);
                    }
                    _ => {
                        directory.hosts.insert(host, provider_key.clone());
                    }
                }
            }
            directory.providers.insert(provider_key, entry.models);
        }
        directory
    }

    /// 解析顺序第三级的查找：先用户 provider 名精确匹配目录 key，未命中且
    /// base_url host 在目录中唯一归属某条目时按该条目回退。model id 先原样
    /// 精确匹配，再大小写不敏感回退；不做模糊或前缀匹配。
    pub(crate) fn limits_for(
        &self,
        provider_name: &str,
        model_id: &str,
        endpoint_host: Option<&str>,
    ) -> Option<ModelTokenLimits> {
        let provider_models = self.providers.get(provider_name).or_else(|| {
            let host = endpoint_host?;
            self.hosts.get(host).and_then(|key| self.providers.get(key))
        })?;
        lookup_model_limits(provider_models, model_id)
    }
}

fn lookup_model_limits(
    models: &BTreeMap<String, ModelTokenLimits>,
    model_id: &str,
) -> Option<ModelTokenLimits> {
    if let Some(limits) = models.get(model_id) {
        return Some(*limits);
    }
    models
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model_id))
        .map(|(_, limits)| *limits)
}

/// 提取 http(s) 端点的小写 host；不可解析或非 http(s) 时返回 None。
pub(crate) fn http_endpoint_host(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// 读取用户配置目录下的元数据缓存（捕获/目录/导入链路的唯一读入口），
/// 返回 TTL 内有效的内存投影。
///
/// 文件缺失、超限、schema 不符、损坏或已过 TTL 时返回空目录——捕获链路
/// 因此保持与无目录来源时完全相同的 fail closed 行为。
pub(crate) fn load_user_metadata_directory(config_directory: &Path) -> ModelMetadataDirectory {
    load_metadata_directory(
        &config_directory.join(crate::METADATA_CACHE_FILE_NAME),
        super::unix_timestamp_seconds(),
    )
}

fn load_metadata_directory(cache_path: &Path, now_unix_seconds: u64) -> ModelMetadataDirectory {
    let Ok(text) = read_bounded_text(cache_path, MAX_DISCOVERY_RESPONSE_BYTES) else {
        return ModelMetadataDirectory::empty();
    };
    let Ok(cache) = serde_json::from_str::<MetadataCacheFile>(&text) else {
        return ModelMetadataDirectory::empty();
    };
    if cache.schema_version != METADATA_CACHE_SCHEMA_VERSION
        || cache.providers.len() > MAX_DISCOVERED_MODEL_IDS
        || cache.providers.iter().any(|(provider_key, entry)| {
            validate_provider_identifier(provider_key, "provider id").is_err()
                || entry.models.len() > MAX_DISCOVERED_MODEL_IDS
                || entry
                    .models
                    .keys()
                    .any(|model_id| validate_model_id(model_id, "model id").is_err())
                || entry.hosts.iter().any(|host| {
                    host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control())
                })
        })
        || cache.fetched_at_unix_seconds > now_unix_seconds
        || now_unix_seconds - cache.fetched_at_unix_seconds > USER_MODELS_CACHE_TTL_SECONDS
    {
        return ModelMetadataDirectory::empty();
    }
    ModelMetadataDirectory::from_cache(cache)
}

/// 把 models.dev api.json 顶层负载投影为缓存 schema。
///
/// 顶层不是 object、providers/models 数量超限等响应级缺陷整体弃用；
/// 单个 provider 或 model 条目损坏只被跳过。
pub(crate) fn project_models_dev_payload(payload: &Value) -> Result<MetadataCacheFile, ()> {
    let entries = payload.as_object().ok_or(())?;
    if entries.len() > MAX_DISCOVERED_MODEL_IDS {
        return Err(());
    }
    let mut providers = BTreeMap::new();
    for (provider_key, provider_value) in entries {
        if validate_provider_identifier(provider_key, "provider id").is_err() {
            continue;
        }
        let Some(models_value) = provider_value.get("models").and_then(Value::as_object) else {
            continue;
        };
        if models_value.len() > MAX_DISCOVERED_MODEL_IDS {
            return Err(());
        }
        let hosts = provider_value
            .get("api")
            .and_then(Value::as_str)
            .and_then(http_endpoint_host)
            .into_iter()
            .collect::<Vec<_>>();
        let mut models = BTreeMap::new();
        for (model_id, model_value) in models_value {
            let Some(limits) = project_model_limits(model_value) else {
                continue;
            };
            if validate_model_id(model_id, "model id").is_err() {
                continue;
            }
            models.insert(model_id.clone(), limits);
        }
        if models.is_empty() {
            continue;
        }
        providers.insert(
            provider_key.clone(),
            MetadataCacheProvider { hosts, models },
        );
    }
    Ok(MetadataCacheFile {
        schema_version: METADATA_CACHE_SCHEMA_VERSION,
        fetched_at_unix_seconds: super::unix_timestamp_seconds(),
        providers,
    })
}

/// 单个模型条目的限额投影：limit 必须为正整数、落在捕获校验上限内且
/// output < context；任何越界条目视为不可用并跳过，保证填充值永远能通过
/// 既有配置校验而不引入新的失败模式。
fn project_model_limits(model_value: &Value) -> Option<ModelTokenLimits> {
    let limit = model_value.get("limit")?;
    let context = positive_u32_within(limit.get("context"), MAX_CONFIGURED_CONTEXT_TOKENS)?;
    let output = positive_u32_within(limit.get("output"), MAX_CONFIGURED_OUTPUT_TOKENS)?;
    (output < context).then_some(ModelTokenLimits { context, output })
}

fn positive_u32_within(value: Option<&Value>, upper_bound: u32) -> Option<u32> {
    let raw = value?.as_u64()?;
    if raw == 0 || raw > u64::from(upper_bound) {
        return None;
    }
    u32::try_from(raw).ok()
}

/// 序列化元数据缓存并强制读取侧字节上限。
///
/// 落盘产物必须始终能被 `load_metadata_directory` 完整接受：超过读取上限
/// 的投影在此拒绝写入（调用方 fail-soft），而不是留下一个读侧永远视为损坏
/// 的缓存文件。
fn serialize_metadata_cache(cache: &MetadataCacheFile) -> Result<String, ()> {
    let text = serde_json::to_string_pretty(cache).map_err(|_| ())?;
    if text.len() > MAX_DISCOVERY_RESPONSE_BYTES {
        return Err(());
    }
    Ok(text)
}

/// 拉取 models.dev 目录并把投影写入独立缓存文件。
///
/// 仅在既有发现刷新链路成功后调用；返回的错误由调用方 fail-soft 忽略，
/// 不影响主流程。响应体与缓存文件的体积上限沿用既有惯例。
pub(crate) fn refresh_metadata_cache(
    cache_path: &Path,
    runtime_handle: &tokio::runtime::Handle,
) -> Result<(), crate::error::ProviderError> {
    use crate::error::ProviderErrorStage;
    let cancellation = CancellationToken::new();
    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
        .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| {
            configuration_error(
                "models.dev metadata request client could not be initialized",
                "metadata_directory_refresh_failed",
            )
        })?;
    let response = block_on_provider_future(
        runtime_handle,
        &cancellation,
        "metadata_directory_request_failed",
        ProviderErrorStage::RequestSend,
        PROVIDER_TIMEOUT_SECONDS,
        || client.get(METADATA_DIRECTORY_URL).send(),
    )?;
    let status = response.status();
    if !status.is_success() {
        return Err(crate::error::ProviderError::from_model_error(
            model_error_from_http_status(status.as_u16(), "models.dev", "metadata"),
        ));
    }
    let body = read_bounded_provider_response_body(
        runtime_handle,
        &cancellation,
        PROVIDER_TIMEOUT_SECONDS,
        response,
    )?;
    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
        configuration_error(
            "models.dev metadata response was not valid JSON",
            "metadata_directory_refresh_failed",
        )
    })?;
    let cache = project_models_dev_payload(&payload).map_err(|_| {
        configuration_error(
            "models.dev metadata response exceeded the projection safety limit",
            "metadata_directory_refresh_failed",
        )
    })?;
    let text = serialize_metadata_cache(&cache).map_err(|_| {
        configuration_error(
            "models.dev metadata projection exceeded the cache size limit",
            "metadata_directory_refresh_failed",
        )
    })?;
    write_json_file(cache_path, &text, false)
}

#[cfg(test)]
mod tests {
    use super::super::unix_timestamp_seconds;
    use super::*;
    use crate::METADATA_CACHE_FILE_NAME;

    fn limit_entry(context: u64, output: u64) -> Value {
        serde_json::json!({ "limit": { "context": context, "output": output } })
    }

    fn sample_payload() -> Value {
        serde_json::json!({
            "deepseek": {
                "api": "https://api.deepseek.com",
                "models": {
                    "deepseek-v4-flash": limit_entry(1_000_000, 384_000),
                    "broken": {}
                }
            },
            "longcat": {
                "api": "https://api.longcat.chat/openai",
                "models": { "LongCat-2.0": limit_entry(1_000_000, 131_072) }
            },
            "empty-provider": { "api": "https://empty.example", "models": {} }
        })
    }

    #[test]
    fn projection_keeps_valid_entries_and_skips_broken_ones() {
        let cache = project_models_dev_payload(&sample_payload()).expect("valid payload");
        assert_eq!(cache.schema_version, METADATA_CACHE_SCHEMA_VERSION);
        assert_eq!(
            cache.providers["deepseek"].models["deepseek-v4-flash"],
            ModelTokenLimits {
                context: 1_000_000,
                output: 384_000
            }
        );
        assert_eq!(
            cache.providers["longcat"].hosts,
            vec!["api.longcat.chat".to_string()]
        );
        assert!(!cache.providers.contains_key("empty-provider"));
        assert!(!cache.providers["deepseek"].models.contains_key("broken"));
    }

    #[test]
    fn projection_rejects_unusable_top_level_payloads() {
        assert!(project_models_dev_payload(&serde_json::json!([])).is_err());
        assert!(project_models_dev_payload(&serde_json::json!("text")).is_err());
        let oversized = serde_json::json!({
            "deepseek": { "models": (0..MAX_DISCOVERED_MODEL_IDS + 1)
                .map(|index| (format!("m{index}"), limit_entry(1000, 100)))
                .collect::<serde_json::Map<_, _>>() }
        });
        assert!(project_models_dev_payload(&oversized).is_err());
    }

    #[test]
    fn projection_drops_limits_outside_capture_validation_bounds() {
        let payload = serde_json::json!({
            "provider": { "models": {
                "zero-context": limit_entry(0, 100),
                "context-over-cap": limit_entry(u64::from(MAX_CONFIGURED_CONTEXT_TOKENS) + 1, 100),
                "output-over-cap": limit_entry(1_000_000, u64::from(MAX_CONFIGURED_OUTPUT_TOKENS) + 1),
                "output-ge-context": limit_entry(1000, 1000),
                "valid": limit_entry(200_000, 8_192)
            } }
        });
        let cache = project_models_dev_payload(&payload).expect("valid payload");
        let models = &cache.providers["provider"].models;
        assert_eq!(
            models["valid"],
            ModelTokenLimits {
                context: 200_000,
                output: 8_192
            }
        );
        assert_eq!(models.len(), 1, "unusable limit entries must be dropped");
    }

    #[test]
    fn serialization_rejects_projections_over_the_read_side_byte_bound() {
        // 超出读取上限的合法投影必须拒绝落盘，而不是留下读侧永远视为损坏
        // 的缓存文件。
        let oversized = MetadataCacheFile {
            schema_version: METADATA_CACHE_SCHEMA_VERSION,
            fetched_at_unix_seconds: 0,
            providers: (0..MAX_DISCOVERED_MODEL_IDS)
                .map(|provider| {
                    (
                        format!("p{provider:04}"),
                        MetadataCacheProvider {
                            hosts: vec![format!("h{provider:04}.example")],
                            models: (0..80)
                                .map(|model| {
                                    (
                                        format!("model-{model:03}"),
                                        ModelTokenLimits {
                                            context: 128_000,
                                            output: 4_096,
                                        },
                                    )
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&oversized).expect("serialize oversized");
        assert!(
            text.len() > MAX_DISCOVERY_RESPONSE_BYTES,
            "test data must exceed the bound"
        );
        assert!(serialize_metadata_cache(&oversized).is_err());

        let small = project_models_dev_payload(&sample_payload()).expect("projection");
        serialize_metadata_cache(&small).expect("a realistic projection stays within the bound");
    }

    fn write_cache(directory: &Path, fetched_at_unix_seconds: u64) -> std::path::PathBuf {
        let path = directory.join(METADATA_CACHE_FILE_NAME);
        let payload = sample_payload();
        let mut cache = project_models_dev_payload(&payload).expect("projection");
        cache.fetched_at_unix_seconds = fetched_at_unix_seconds;
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&cache).expect("serialize cache"),
        )
        .expect("write cache");
        path
    }

    #[test]
    fn fresh_cache_serves_exact_and_case_insensitive_model_lookups() {
        let directory = tempfile::tempdir().expect("cache directory");
        let now = unix_timestamp_seconds();
        let path = write_cache(directory.path(), now);

        let metadata = load_metadata_directory(&path, now);
        let deepseek = metadata
            .limits_for("deepseek", "deepseek-v4-flash", None)
            .expect("exact hit");
        assert_eq!(
            deepseek,
            ModelTokenLimits {
                context: 1_000_000,
                output: 384_000
            }
        );

        let longcat = metadata
            .limits_for("longcat", "longcat-2.0", None)
            .expect("case-insensitive model id fallback");
        assert_eq!(
            longcat,
            ModelTokenLimits {
                context: 1_000_000,
                output: 131_072
            }
        );

        assert!(
            metadata
                .limits_for("longcat", "LongCat-2.0", None)
                .is_some()
        );
        assert!(
            metadata
                .limits_for("unknown", "deepseek-v4-flash", None)
                .is_none()
        );
        assert!(
            metadata
                .limits_for("deepseek", "missing-model", None)
                .is_none()
        );
    }

    #[test]
    fn unique_api_host_enables_secondary_lookup_and_ambiguous_hosts_are_rejected() {
        let directory = tempfile::tempdir().expect("cache directory");
        let now = unix_timestamp_seconds();
        let path = write_cache(directory.path(), now);

        let metadata = load_metadata_directory(&path, now);
        let via_host = metadata
            .limits_for("my-deepseek", "deepseek-v4-flash", Some("api.deepseek.com"))
            .expect("unique host maps to the directory entry");
        assert_eq!(via_host.output, 384_000);
        assert!(
            metadata
                .limits_for("other", "m", Some("unmapped.example"))
                .is_none()
        );

        let colliding = serde_json::json!({
            "first": { "api": "https://shared.example/v1", "models": { "m1": limit_entry(10_000, 1_000) } },
            "second": { "api": "https://shared.example/v2", "models": { "m2": limit_entry(20_000, 2_000) } }
        });
        let cache = project_models_dev_payload(&colliding).expect("projection");
        let shared = ModelMetadataDirectory::from_cache(cache);
        assert!(
            shared
                .limits_for("third-party", "m2", Some("shared.example"))
                .is_none()
        );
    }

    #[test]
    fn missing_or_expired_or_corrupt_cache_stays_empty() {
        let directory = tempfile::tempdir().expect("cache directory");
        let now = unix_timestamp_seconds();

        let missing = load_metadata_directory(&directory.path().join("absent.json"), now);
        assert!(
            missing
                .limits_for("deepseek", "deepseek-v4-flash", None)
                .is_none()
        );

        let expired_path = write_cache(directory.path(), now - USER_MODELS_CACHE_TTL_SECONDS - 1);
        let expired = load_metadata_directory(&expired_path, now);
        assert!(
            expired
                .limits_for("deepseek", "deepseek-v4-flash", None)
                .is_none()
        );

        let corrupt = directory.path().join(METADATA_CACHE_FILE_NAME);
        std::fs::write(&corrupt, b"not-json").expect("write corrupt cache");
        assert!(
            load_metadata_directory(&corrupt, now)
                .limits_for("deepseek", "deepseek-v4-flash", None)
                .is_none()
        );

        let oversized = directory.path().join("oversized.json");
        std::fs::write(&oversized, vec![b'x'; MAX_DISCOVERY_RESPONSE_BYTES + 1])
            .expect("write oversized cache");
        assert!(
            load_metadata_directory(&oversized, now)
                .limits_for("deepseek", "deepseek-v4-flash", None)
                .is_none()
        );

        // 时钟回拨（fetched_at 在未来）同样视为过期。
        let future_path = write_cache(directory.path(), now + 60);
        assert!(
            load_metadata_directory(&future_path, now)
                .limits_for("deepseek", "deepseek-v4-flash", None)
                .is_none()
        );
    }

    #[test]
    #[ignore = "requires real network access to https://models.dev/api.json"]
    fn real_directory_refresh_populates_known_providers() {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let runtime = RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime")
        });
        let directory = tempfile::tempdir().expect("cache directory");
        let path = directory.path().join(METADATA_CACHE_FILE_NAME);
        refresh_metadata_cache(&path, runtime.handle()).expect("real refresh must succeed");
        let cache_bytes = std::fs::metadata(&path).expect("cache file metadata").len();
        println!(
            "live metadata-cache.json bytes={cache_bytes} read_bound={MAX_DISCOVERY_RESPONSE_BYTES}"
        );
        assert!(
            (cache_bytes as usize) <= MAX_DISCOVERY_RESPONSE_BYTES,
            "the live projection must stay within the read-side bound"
        );

        let metadata = load_metadata_directory(&path, unix_timestamp_seconds());
        let deepseek = metadata
            .limits_for("deepseek", "deepseek-v4-flash", None)
            .expect("deepseek-v4-flash in the live directory");
        assert_eq!(deepseek.context, 1_000_000);
        assert_eq!(deepseek.output, 384_000);
        let longcat = metadata
            .limits_for("longcat", "LongCat-2.0", None)
            .expect("LongCat-2.0 in the live directory");
        assert_eq!(longcat.context, 1_000_000);
        assert_eq!(longcat.output, 131_072);
    }
}
