//! Dotenv 环境变量导入至用户配置分层。

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::auth::{
    UserAuthFile, UserAuthProvider, acquire_config_writer_lock, new_auth_generation_name,
    write_new_auth_generation,
};
use super::catalog::user_model_override_is_selectable;
use super::{
    USER_CONFIG_FILE_NAME, UserConfigData, UserConfigFile, UserConfigProvider,
    ensure_no_reparse_components, read_user_config_data, user_config_directory_result,
    user_config_error,
};
use crate::config::filesystem::write_json_file;
use crate::config::schema::{
    ProviderConfigSource, validate_identifier, validate_model_id, validate_provider_identifier,
};
use crate::config::{
    find_import_env_file, normalized_endpoint_identity, parse_model_selector,
    read_import_env_layer, validate_base_url, validate_provider_value,
};
use crate::error::ProviderError;
use crate::{DEFAULT_PROVIDER_NAME, ENV_API_KEY, ENV_MODEL, USER_AUTH_SCHEMA_VERSION};

/// Outcome of importing a dotenv file into the user-level split config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserConfigImportResult {
    pub config_path: String,
    pub auth_path: String,
    pub provider_name: String,
    pub default_selector: Option<String>,
    pub selectable: bool,
}

pub fn import_env_to_user_config(
    path: Option<&Path>,
) -> Result<UserConfigImportResult, ProviderError> {
    let env_path = match path {
        Some(path) if path.is_file() => path.to_path_buf(),
        Some(_) => return Err(user_config_error("explicit dotenv file could not be read")),
        None => {
            let current_dir = std::env::current_dir()
                .map_err(|_| user_config_error("current directory could not be read"))?;
            find_import_env_file(&current_dir)
                .ok_or_else(|| user_config_error("no .env file was found"))?
        }
    };
    let layer = read_import_env_layer(&env_path);
    let base_url = layer
        .base_url
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_BASE_URL is required for import-env"))?;
    let api_key = layer
        .api_key
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_API_KEY is required for import-env"))?;
    let model_value = layer
        .model_name
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("SINGULARITY_MODEL is required for import-env"))?;
    validate_base_url(Some(&base_url), Some(ProviderConfigSource::UserConfigFile))?;
    validate_provider_value(
        Some(&api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let provider_name = layer
        .provider_name
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
    validate_provider_identifier(&provider_name, "provider id")?;
    validate_provider_value(
        Some(&model_value),
        ENV_MODEL,
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let (default_selector, model_name) = parse_import_model_selector(&model_value, &provider_name)?;
    let directory = user_config_directory_result()?
        .ok_or_else(|| user_config_error("user config directory is unavailable"))?;
    ensure_no_reparse_components(&directory, true)?;
    std::fs::create_dir_all(&directory)
        .map_err(|_| user_config_error("user provider config directory could not be created"))?;
    ensure_no_reparse_components(&directory, false)?;
    let empty_existing = || UserConfigData {
        directory: directory.clone(),
        config: UserConfigFile {
            version: 1,
            default_provider: None,
            default_model: None,
            auth_generation: None,
            providers: BTreeMap::new(),
        },
        auth: UserAuthFile {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        },
    };
    let existing_before_lock = read_user_config_data()?;
    reject_import_endpoint_change(existing_before_lock.as_ref(), &provider_name, &base_url)?;
    let _writer_lock = acquire_config_writer_lock(&directory)?;
    let existing = read_user_config_data()?.unwrap_or_else(empty_existing);
    reject_import_endpoint_change(Some(&existing), &provider_name, &base_url)?;
    let mut config = existing.config;
    config.version = 1;
    config.default_provider = Some(provider_name.clone());
    config.default_model = Some(default_selector.clone());
    let provider = config
        .providers
        .entry(provider_name.clone())
        .or_insert_with(|| UserConfigProvider {
            base_url: base_url.clone(),
            models: BTreeMap::new(),
        });
    provider.base_url = base_url.clone();
    let model = provider.models.entry(model_name.clone()).or_default();
    if let Some(variant) = parse_model_selector(&default_selector)?.reasoning_effort
        && !model.reasoning_variants.contains_key(variant)
    {
        return Err(user_config_error(
            "reasoning variant must already be explicitly declared before import",
        ));
    }
    let mut auth = existing.auth;
    auth.schema_version = USER_AUTH_SCHEMA_VERSION;
    auth.providers
        .insert(provider_name.clone(), UserAuthProvider { api_key });
    validate_imported_user_config(&config, &auth)?;
    let selectable = imported_model_is_selectable(
        &config,
        &auth,
        &provider_name,
        &model_name,
        parse_model_selector(&default_selector)?.reasoning_effort,
    );
    let auth_text = serde_json::to_string_pretty(&auth)
        .map_err(|_| user_config_error("user provider auth could not be serialized"))?;
    let generation = new_auth_generation_name();
    config.auth_generation = Some(generation.clone());
    let config_text = serde_json::to_string_pretty(&config)
        .map_err(|_| user_config_error("user provider config could not be serialized"))?;
    let config_path = directory.join(USER_CONFIG_FILE_NAME);
    let auth_path = write_new_auth_generation(&directory, &generation, &auth_text)?;
    if let Err(error) = write_json_file(&config_path, &config_text, false) {
        let _ = std::fs::remove_file(&auth_path);
        return Err(error);
    }
    Ok(UserConfigImportResult {
        config_path: config_path.to_string_lossy().to_string(),
        auth_path: auth_path.to_string_lossy().to_string(),
        provider_name,
        default_selector: Some(default_selector),
        selectable,
    })
}

fn reject_import_endpoint_change(
    existing: Option<&UserConfigData>,
    provider_name: &str,
    base_url: &str,
) -> Result<(), ProviderError> {
    let Some(existing_provider) =
        existing.and_then(|data| data.config.providers.get(provider_name))
    else {
        return Ok(());
    };
    let old_identity = normalized_endpoint_identity(&existing_provider.base_url)?;
    let new_identity = normalized_endpoint_identity(base_url)?;
    if old_identity != new_identity {
        return Err(user_config_error(
            "provider id already points to a different endpoint; use a distinct provider id or edit config explicitly",
        ));
    }
    Ok(())
}

pub(crate) fn parse_import_model_selector(
    model_value: &str,
    provider_name: &str,
) -> Result<(String, String), ProviderError> {
    let provider_prefix = format!("{provider_name}/");
    if model_value.starts_with(&provider_prefix) {
        let parsed = parse_model_selector(model_value)?;
        validate_provider_identifier(parsed.provider_name, "provider id")?;
        validate_model_id(parsed.model_name, "model id")?;
        if let Some(variant) = parsed.reasoning_effort {
            validate_identifier(variant, "reasoning variant")?;
        }
        if parsed.provider_name != provider_name {
            return Err(user_config_error(
                "SINGULARITY_MODEL provider does not match SINGULARITY_MODEL_PROVIDER",
            ));
        }
        Ok((model_value.to_string(), parsed.model_name.to_string()))
    } else {
        validate_model_id(model_value, "model id")?;
        Ok((
            format!("{provider_name}/{model_value}"),
            model_value.to_string(),
        ))
    }
}

fn validate_imported_user_config(
    config: &UserConfigFile,
    auth: &UserAuthFile,
) -> Result<(), ProviderError> {
    let default_provider = config
        .default_provider
        .as_deref()
        .ok_or_else(|| user_config_error("user provider config must declare default_provider"))?;
    let default_model = config
        .default_model
        .as_deref()
        .ok_or_else(|| user_config_error("user provider config must declare default_model"))?;
    let parsed = parse_model_selector(default_model)?;
    if parsed.provider_name != default_provider {
        return Err(user_config_error(
            "default_provider does not match default_model",
        ));
    }
    let provider = config
        .providers
        .get(default_provider)
        .ok_or_else(|| user_config_error("default_model references an unknown provider"))?;
    validate_provider_identifier(default_provider, "provider id")?;
    validate_base_url(
        Some(&provider.base_url),
        Some(ProviderConfigSource::UserConfigFile),
    )?;
    let model = provider
        .models
        .get(parsed.model_name)
        .ok_or_else(|| user_config_error("default_model references an unknown model"))?;
    validate_model_id(parsed.model_name, "model id")?;
    if let Some(variant) = parsed.reasoning_effort
        && !model.reasoning_variants.contains_key(variant)
    {
        return Err(user_config_error(
            "default_model references an unknown reasoning variant",
        ));
    }
    let api_key = auth
        .providers
        .get(default_provider)
        .map(|provider| provider.api_key.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| user_config_error("default provider api_key is required"))?;
    validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )
}

fn imported_model_is_selectable(
    config: &UserConfigFile,
    auth: &UserAuthFile,
    provider_name: &str,
    model_name: &str,
    _reasoning_variant: Option<&str>,
) -> bool {
    let Some(provider) = config.providers.get(provider_name) else {
        return false;
    };
    let Some(model) = provider.models.get(model_name) else {
        return false;
    };
    if validate_base_url(
        Some(&provider.base_url),
        Some(ProviderConfigSource::UserConfigFile),
    )
    .is_err()
    {
        return false;
    }
    let Some(api_key) = auth
        .providers
        .get(provider_name)
        .map(|provider| provider.api_key.as_str())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    if validate_provider_value(
        Some(api_key),
        ENV_API_KEY,
        Some(ProviderConfigSource::UserConfigFile),
    )
    .is_err()
    {
        return false;
    }
    user_model_override_is_selectable(provider_name, model_name, model)
}
