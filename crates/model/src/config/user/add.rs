//! `sg config add` 的一次性 provider 录入：校验 → `/models` 发现 →
//! 限额 enrichment（models.dev 元数据 > 内置表 > 保守默认）→ 持久化
//! config.json providers 段与 auth.v1.json。

use std::collections::BTreeMap;

use super::auth::{UserAuthFile, UserAuthProvider, user_auth_file_path};
use super::metadata::{http_endpoint_host, load_user_metadata_directory, refresh_metadata_cache};
use super::{
    UserConfigData, UserConfigFile, UserConfigModel, UserConfigProvider,
    acquire_config_writer_lock, ensure_no_reparse_point, read_user_config_data,
    user_config_directory_result, user_config_error,
};
use crate::builtin_models::builtin_model;
use crate::config::filesystem::write_json_file;
use crate::config::schema::{validate_model_id, validate_provider_identifier};
use crate::config::{validate_base_url, validate_provider_value};
use crate::error::ProviderError;
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_PROVIDER_NAME, ENV_API_KEY,
    METADATA_CACHE_FILE_NAME, ProviderConfigSource, USER_AUTH_SCHEMA_VERSION,
    USER_CONFIG_FILE_NAME,
};

/// `sg config add` 的持久化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddProviderResult {
    pub config_path: String,
    pub auth_path: String,
    pub provider_name: String,
    pub default_selector: Option<String>,
    pub models_written: usize,
}

/// 对 `base_url` 端点执行 `/models` 发现（网络调用）；失败时返回带
/// provider 诊断的错误，由 CLI 原样呈现给用户。
pub fn discover_provider_model_ids(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, ProviderError> {
    validate_base_url(Some(base_url), Some(ProviderConfigSource::UserConfigFile))?;
    validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            user_config_error(format!("failed to start discovery runtime: {error}"))
        })?;
    let config = crate::OpenAiProviderConfig {
        provider_name: DEFAULT_PROVIDER_NAME.to_string(),
        model_name: "models".to_string(),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        source: ProviderConfigSource::UserConfigFile,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    };
    crate::OpenAiProvider::new(config, runtime.handle().clone())
        .and_then(|provider| provider.discover_model_ids())
        .and_then(super::catalog::validate_discovered_model_ids)
}

/// 刷新 models.dev 元数据缓存（网络，fail-soft：失败记录诊断并返回 false）。
/// `sg config add` 在持久化前调用一次，让限额 enrichment 尽可能命中目录值。
pub fn refresh_model_metadata() -> bool {
    let Ok(Some(directory)) = user_config_directory_result() else {
        return false;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let metadata_cache_path = directory.join(METADATA_CACHE_FILE_NAME);
    match refresh_metadata_cache(&metadata_cache_path, runtime.handle()) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "model metadata refresh failed ({}): {error}",
                metadata_cache_path.display()
            );
            false
        }
    }
}

/// 持久化一个 provider 条目：写 config.json providers 段与 auth.v1.json。
///
/// 模型限额取自 models.dev 元数据缓存（同目录 TTL 有效缓存，缺失时先尝试
/// 刷新一次，网络失败 fail-soft）或内置表，均未命中时回落保守默认。
/// 新 provider 成为默认（default_provider/default_model 指向其首个模型）。
pub fn add_configured_provider(
    name: &str,
    base_url: &str,
    api_key: &str,
    model_ids: Vec<String>,
) -> Result<AddProviderResult, ProviderError> {
    validate_provider_identifier(name, "provider id")?;
    validate_base_url(Some(base_url), Some(ProviderConfigSource::UserConfigFile))?;
    validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    if model_ids.is_empty() {
        return Err(user_config_error(
            "provider models discovery returned no model ids",
        ));
    }
    let directory = user_config_directory_result()?
        .ok_or_else(|| user_config_error("user config directory is unavailable"))?;
    ensure_no_reparse_point(&directory, true)?;
    std::fs::create_dir_all(&directory)
        .map_err(|_| user_config_error("user provider config directory could not be created"))?;
    ensure_no_reparse_point(&directory, false)?;

    // 限额 enrichment 只读本地 models.dev 元数据缓存（写路径由 CLI 在调用
    // 前显式刷新）；缓存缺失时内置表与保守默认兜底。
    let metadata_directory = load_user_metadata_directory(&directory);
    let endpoint_host = http_endpoint_host(base_url);

    let mut models = BTreeMap::new();
    for model_id in &model_ids {
        validate_model_id(model_id, "model id")?;
        let directory_limits =
            metadata_directory.limits_for(name, model_id, endpoint_host.as_deref());
        let builtin = builtin_model(name, model_id);
        let max_context_tokens = directory_limits
            .map(|limits| limits.context)
            .or_else(|| builtin.map(|entry| entry.context_window))
            .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS);
        let max_output_tokens = directory_limits
            .map(|limits| limits.output)
            .or_else(|| builtin.map(|entry| entry.max_output_tokens))
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        models.insert(
            model_id.clone(),
            UserConfigModel {
                api_protocol: Some("chat".to_string()),
                max_context_tokens: Some(max_context_tokens),
                max_output_tokens: Some(max_output_tokens),
                ..UserConfigModel::default()
            },
        );
    }

    let empty_existing = || UserConfigData {
        directory: directory.clone(),
        config: UserConfigFile {
            version: 1,
            default_provider: None,
            default_model: None,
            providers: BTreeMap::new(),
        },
        auth: UserAuthFile {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        },
    };
    let _writer_lock = acquire_config_writer_lock(&directory)?;
    let existing = read_user_config_data()?.unwrap_or_else(empty_existing);
    let mut config = existing.config;
    config.version = 1;
    config.providers.insert(
        name.to_string(),
        UserConfigProvider {
            base_url: base_url.to_string(),
            models,
        },
    );
    let default_selector = if config
        .default_provider
        .as_deref()
        .is_some_and(|provider| config.providers.contains_key(provider))
        && config.default_model.is_some()
    {
        config.default_model.clone()
    } else {
        let first_model = config
            .providers
            .get(name)
            .expect("inserted provider")
            .models
            .keys()
            .next()
            .cloned()
            .expect("model ids checked non-empty");
        config.default_provider = Some(name.to_string());
        let selector = format!("{name}/{first_model}");
        config.default_model = Some(selector.clone());
        Some(selector)
    };
    let mut auth = existing.auth;
    auth.schema_version = USER_AUTH_SCHEMA_VERSION;
    auth.providers.insert(
        name.to_string(),
        UserAuthProvider {
            api_key: api_key.to_string(),
        },
    );
    let auth_text = serde_json::to_string_pretty(&auth)
        .map_err(|_| user_config_error("user provider auth could not be serialized"))?;
    let config_text = serde_json::to_string_pretty(&config)
        .map_err(|_| user_config_error("user provider config could not be serialized"))?;
    let config_path = directory.join(USER_CONFIG_FILE_NAME);
    let auth_path = user_auth_file_path(&directory)?;
    // 与 import 相同的写序：auth 先写（config 写入失败时新凭据对旧 config
    // 无害），两个文件各自经临时文件 + 同卷原子改名落盘。
    write_json_file(&auth_path, &auth_text, true)?;
    write_json_file(&config_path, &config_text, false)?;
    Ok(AddProviderResult {
        config_path: config_path.to_string_lossy().to_string(),
        auth_path: auth_path.to_string_lossy().to_string(),
        provider_name: name.to_string(),
        default_selector,
        models_written: model_ids.len(),
    })
}
