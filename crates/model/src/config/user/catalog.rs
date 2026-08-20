//! 用户级模型目录、发现缓存与脱敏清单。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::filesystem::{read_bounded_text, write_json_file};
use crate::config::schema::{
    deserialize_unique_map, deserialize_unique_vec, validate_model_id,
    validate_provider_identifier,
};
use crate::config::{
    configured_model_from_user_file, normalized_endpoint_identity, validate_base_url,
    validate_provider_value,
};
use super::{UserConfigModel, read_user_config_data};
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ENV_API_KEY,
    MAX_DISCOVERED_MODEL_IDS, OpenAiProviderConfig, ProviderConfigSource,
};
use crate::error::ProviderError;
use crate::transport::OpenAiProvider;

pub const USER_MODELS_CACHE_FILE_NAME: &str = "models-cache.json";
pub const USER_MODELS_CACHE_SCHEMA_VERSION: u32 = 1;
pub const USER_MODELS_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;

/// State of one provider's `/models` discovery record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelDiscoveryStatus {
    Fresh,
    Stale,
    Unavailable,
    NotConfigured,
}

/// Result of reading or refreshing the optional user model discovery cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelCacheStatus {
    NotPresent,
    Valid,
    Invalid,
    ReadFailed,
    WriteFailed,
}

/// A discovered model id and whether an explicit capability override makes it
/// safe to select for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalogEntry {
    pub id: String,
    pub discovered: bool,
    pub explicit: bool,
    pub selectable: bool,
    pub max_context_tokens: Option<u32>,
    pub reasoning_variants: Vec<String>,
    pub default_variant: Option<String>,
}

/// Redacted user-level provider catalog returned by `sg config models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserProviderModelCatalog {
    pub provider_name: String,
    pub base_url_present: bool,
    pub api_key_present: bool,
    pub discovery: ModelDiscoveryStatus,
    pub models: Vec<UserModelCatalogEntry>,
    pub error: Option<String>,
}

/// Redacted user-level model catalog. It never contains a base URL or secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalog {
    pub default_selector: Option<String>,
    pub cache_status: ModelCacheStatus,
    pub providers: Vec<UserProviderModelCatalog>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserModelsCacheFile {
    pub schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub providers: BTreeMap<String, UserModelsCacheRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserModelsCacheRecord {
    pub endpoint_sha256: String,
    pub fetched_at_unix_seconds: u64,
    #[serde(deserialize_with = "deserialize_unique_vec")]
    pub model_ids: Vec<String>,
}

pub struct ModelsCacheLoad {
    pub cache: UserModelsCacheFile,
    pub status: ModelCacheStatus,
}

pub fn endpoint_fingerprint(base_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let identity = normalized_endpoint_identity(base_url).unwrap_or_else(|_| base_url.to_string());
    digest.update(identity.as_bytes());
    format!("{:x}", digest.finalize())
}

pub fn user_model_override_is_selectable(
    provider_name: &str,
    model_name: &str,
    model: &UserConfigModel,
) -> bool {
    configured_model_from_user_file(provider_name, model_name, model).is_ok()
}

pub fn unix_timestamp_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub fn load_models_cache(path: &Path) -> ModelsCacheLoad {
    let empty_cache = || UserModelsCacheFile {
        schema_version: USER_MODELS_CACHE_SCHEMA_VERSION,
        providers: BTreeMap::new(),
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::NotPresent,
            };
        }
        Ok(metadata) if metadata.is_file() => {}
        _ => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::ReadFailed,
            };
        }
    }
    let text = match read_bounded_text(path, crate::MAX_DISCOVERY_RESPONSE_BYTES) {
        Ok(text) => text,
        Err(error) => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: if error.is_invalid_data() {
                    ModelCacheStatus::Invalid
                } else {
                    ModelCacheStatus::ReadFailed
                },
            };
        }
    };
    let cache: UserModelsCacheFile = match serde_json::from_str(&text) {
        Ok(cache) => cache,
        Err(_) => {
            return ModelsCacheLoad {
                cache: empty_cache(),
                status: ModelCacheStatus::Invalid,
            };
        }
    };
    if cache.schema_version != USER_MODELS_CACHE_SCHEMA_VERSION {
        return ModelsCacheLoad {
            cache: empty_cache(),
            status: ModelCacheStatus::Invalid,
        };
    }
    if cache.providers.len() > crate::MAX_DISCOVERED_MODEL_IDS
        || cache.providers.iter().any(|(provider_name, record)| {
            validate_provider_identifier(provider_name, "provider id").is_err()
                || record.endpoint_sha256.len() != 64
                || !record
                    .endpoint_sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
                || record.model_ids.len() > crate::MAX_DISCOVERED_MODEL_IDS
                || record
                    .model_ids
                    .iter()
                    .any(|model_id| validate_model_id(model_id, "model id").is_err())
        })
    {
        return ModelsCacheLoad {
            cache: empty_cache(),
            status: ModelCacheStatus::Invalid,
        };
    }
    ModelsCacheLoad {
        cache,
        status: ModelCacheStatus::Valid,
    }
}

pub fn validate_discovered_model_ids(model_ids: Vec<String>) -> Result<Vec<String>, ProviderError> {
    if model_ids.len() > crate::MAX_DISCOVERED_MODEL_IDS {
        return Err(crate::config::configuration_error(
            "provider models response exceeded the model id safety limit",
            "provider_configuration_invalid",
        ));
    }
    let mut seen = BTreeSet::new();
    for model_id in &model_ids {
        validate_model_id(model_id, "discovered model id")?;
        if !seen.insert(model_id) {
            return Err(crate::config::configuration_error(
                "provider models response contained duplicate model ids",
                "provider_configuration_invalid",
            ));
        }
    }
    if model_ids.is_empty() {
        return Err(crate::config::configuration_error(
            "provider models response did not contain model ids",
            "provider_configuration_invalid",
        ));
    }
    Ok(model_ids)
}

fn public_diagnostic(error: &ProviderError) -> String {
    error
        .error
        .message
        .chars()
        .map(|character| match character {
            '\r' => ' ',
            '\n' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

/// Read and, when stale or requested, refresh the user-level `/models` ids.
pub fn read_user_model_catalog(refresh: bool) -> Result<UserModelCatalog, ProviderError> {
    let Some(user_config) = read_user_config_data()? else {
        return Ok(UserModelCatalog {
            default_selector: None,
            cache_status: ModelCacheStatus::NotPresent,
            providers: Vec::new(),
        });
    };
    let cache_path = user_config.directory.join(USER_MODELS_CACHE_FILE_NAME);
    let cache_load = load_models_cache(&cache_path);
    let mut cache = cache_load.cache;
    let mut cache_status = cache_load.status;
    let mut cache_changed = false;
    let now = unix_timestamp_seconds();
    let mut provider_catalogs = Vec::new();
    for (provider_name, provider_file) in &user_config.config.providers {
        if validate_provider_identifier(provider_name, "provider id").is_err() {
            cache_status = ModelCacheStatus::Invalid;
            continue;
        }
        let mut diagnostics = Vec::new();
        let base_url_valid = match validate_base_url(
            Some(&provider_file.base_url),
            Some(ProviderConfigSource::UserConfigFile),
        ) {
            Ok(()) => true,
            Err(_) => {
                diagnostics.push("provider endpoint is invalid".to_string());
                false
            }
        };
        let api_key = user_config
            .auth
            .providers
            .get(provider_name)
            .map(|provider| provider.api_key.clone())
            .filter(|value| !value.is_empty());
        let auth_valid = api_key.as_deref().is_some_and(|api_key| {
            validate_provider_value(
                Some(api_key),
                ENV_API_KEY,
                Some(ProviderConfigSource::UserConfigFile),
            )
            .is_ok()
        });
        if api_key.is_some() && !auth_valid {
            diagnostics.push("provider authentication is invalid".to_string());
        }
        let explicit_ids = provider_file
            .models
            .keys()
            .collect::<BTreeSet<_>>();
        if explicit_ids.iter().any(|id| {
            provider_file
                .models
                .get(*id)
                .is_some_and(|model| !user_model_override_is_selectable(provider_name, id, model))
        }) {
            diagnostics.push("one or more model overrides are incomplete or invalid".to_string());
        }
        let endpoint_hash = if base_url_valid {
            endpoint_fingerprint(&provider_file.base_url)
        } else {
            String::new()
        };
        let cached_ids = cache
            .providers
            .get(provider_name)
            .filter(|record| {
                base_url_valid
                    && record.endpoint_sha256 == endpoint_hash
                    && record.model_ids.len() <= MAX_DISCOVERED_MODEL_IDS
            })
            .map(|record| record.model_ids.clone());
        let cached_fetched_at = cache
            .providers
            .get(provider_name)
            .filter(|record| {
                base_url_valid
                    && record.endpoint_sha256 == endpoint_hash
                    && record.model_ids.len() <= MAX_DISCOVERED_MODEL_IDS
            })
            .map(|record| record.fetched_at_unix_seconds);
        let fresh = cached_fetched_at.is_some_and(|fetched_at| {
            !refresh && fetched_at <= now && now - fetched_at <= USER_MODELS_CACHE_TTL_SECONDS
        });
        let cached_ids_for_fallback = cached_ids.clone();
        let had_cached_ids = cached_ids_for_fallback.is_some();
        let (discovered_ids, discovery, discovery_error) =
            if !base_url_valid || api_key.is_none() || !auth_valid {
                (
                    if base_url_valid {
                        cached_ids.unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    if base_url_valid {
                        ModelDiscoveryStatus::NotConfigured
                    } else {
                        ModelDiscoveryStatus::Unavailable
                    },
                    None,
                )
            } else if fresh {
                (
                    cached_ids.unwrap_or_default(),
                    ModelDiscoveryStatus::Fresh,
                    None,
                )
            } else {
                let discovery_config = OpenAiProviderConfig {
                    provider_name: provider_name.clone(),
                    model_name: "models".to_string(),
                    base_url: provider_file.base_url.clone(),
                    api_key: api_key.clone().unwrap_or_default(),
                    source: ProviderConfigSource::UserConfigFile,
                    max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                    max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                };
                match OpenAiProvider::new(discovery_config)
                    .and_then(|provider| provider.discover_model_ids())
                    .and_then(validate_discovered_model_ids)
                {
                    Ok(model_ids) => {
                        cache.providers.insert(
                            provider_name.clone(),
                            UserModelsCacheRecord {
                                endpoint_sha256: endpoint_hash,
                                fetched_at_unix_seconds: now,
                                model_ids: model_ids.clone(),
                            },
                        );
                        cache_changed = true;
                        (model_ids, ModelDiscoveryStatus::Fresh, None)
                    }
                    Err(error) => (
                        cached_ids_for_fallback.unwrap_or_default(),
                        if had_cached_ids {
                            ModelDiscoveryStatus::Stale
                        } else {
                            ModelDiscoveryStatus::Unavailable
                        },
                        Some(public_diagnostic(&error)),
                    ),
                }
            };
        if let Some(error) = discovery_error {
            diagnostics.push(error);
        }
        let discovered_set = discovered_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut model_entries = Vec::new();
        for id in &discovered_ids {
            let model_override = provider_file.models.get(id);
            let selectable = user_model_override_is_selectable(
                provider_name,
                id,
                model_override.unwrap_or(&UserConfigModel::default()),
            );
            model_entries.push(UserModelCatalogEntry {
                id: id.clone(),
                discovered: true,
                explicit: model_override.is_some(),
                selectable,
                max_context_tokens: model_override.and_then(|model| model.max_context_tokens),
                reasoning_variants: model_override
                    .map(|model| model.reasoning_variants.keys().cloned().collect())
                    .unwrap_or_default(),
                default_variant: model_override.and_then(|model| model.default_variant.clone()),
            });
        }
        for (id, model_override) in &provider_file.models {
            if !discovered_set.contains(id) {
                let selectable =
                    user_model_override_is_selectable(provider_name, id, model_override);
                model_entries.push(UserModelCatalogEntry {
                    id: id.clone(),
                    discovered: false,
                    explicit: true,
                    selectable,
                    max_context_tokens: model_override.max_context_tokens,
                    reasoning_variants: model_override
                        .reasoning_variants
                        .keys()
                        .cloned()
                        .collect(),
                    default_variant: model_override.default_variant.clone(),
                });
            }
        }
        provider_catalogs.push(UserProviderModelCatalog {
            provider_name: provider_name.clone(),
            base_url_present: base_url_valid,
            api_key_present: auth_valid,
            discovery,
            models: model_entries,
            error: (!diagnostics.is_empty()).then(|| diagnostics.join("; ")),
        });
    }
    if cache_changed {
        if let Ok(cache_text) = serde_json::to_string_pretty(&cache) {
            let _ = write_json_file(&cache_path, &cache_text, false);
        }
    }
    let default_selector = user_config.config.default_model.clone();
    Ok(UserModelCatalog {
        default_selector,
        cache_status,
        providers: provider_catalogs,
    })
}
