//! 用户级配置与认证接缝：用户配置与认证保持同一生命周期——读取、校验
//! 并暴露给父模块的单一事实源。

pub(crate) mod auth;

pub(crate) use auth::*;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::schema::{ModelsFileReasoningVariant, deserialize_unique_map};
use crate::error::ProviderError;
use crate::{USER_AUTH_FILE_NAME, USER_CONFIG_FILE_NAME};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigFile {
    #[serde(default = "default_user_config_version")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) default_provider: Option<String>,
    #[serde(default)]
    pub(crate) default_model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) providers: BTreeMap<String, UserConfigProvider>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigProvider {
    pub(crate) base_url: String,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigModel {
    #[serde(default)]
    pub(crate) api_protocol: Option<String>,
    #[serde(default)]
    pub(crate) max_context_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    #[serde(default)]
    pub(crate) default_variant: Option<String>,
    #[serde(default)]
    pub(crate) tool_reasoning_history: Option<String>,
    #[serde(default)]
    pub(crate) supports_developer_role: Option<bool>,
    #[serde(default)]
    pub(crate) supports_tool_choice: Option<bool>,
    #[serde(default)]
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    #[serde(default)]
    pub(crate) requires_assistant_content_for_tool_calls: bool,
    #[serde(default)]
    pub(crate) thinking_wire_format: Option<String>,
}

pub(crate) fn default_user_config_version() -> u32 {
    1
}

pub(crate) fn user_config_error(message: impl Into<String>) -> ProviderError {
    super::configuration_error(message, "provider_configuration_invalid")
}

#[derive(Clone)]
pub(crate) struct UserConfigData {
    pub(crate) config: UserConfigFile,
    pub(crate) auth: UserAuthFile,
}

/// 解析所有工作树共享的用户级目录。
pub(crate) fn user_config_directory_result() -> Result<Option<PathBuf>, ProviderError> {
    let Some((home, explicit)) = singularity_core::user_home_base_from_env() else {
        return Ok(None);
    };
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Err(user_config_error(
            "SINGULARITY_HOME must be a non-empty absolute path",
        ));
    }
    let home = normalize_absolute_path(&home)?;
    if explicit {
        Ok(Some(home))
    } else {
        Ok(Some(home.join(singularity_core::SINGULARITY_DIR_NAME)))
    }
}

pub(crate) fn normalize_absolute_path(path: &Path) -> Result<PathBuf, ProviderError> {
    if !path.is_absolute() {
        return Err(user_config_error(
            "user config directory must be an absolute path",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if !normalized.is_absolute() || normalized.as_os_str().is_empty() {
        return Err(user_config_error(
            "user config directory could not be normalized",
        ));
    }
    Ok(normalized)
}

pub(crate) fn read_user_config_data() -> Result<Option<UserConfigData>, ProviderError> {
    let Some(directory) = user_config_directory_result()? else {
        return Ok(None);
    };
    read_user_config_data_from_directory(directory)
}

pub(crate) fn read_user_config_data_from_directory(
    directory: PathBuf,
) -> Result<Option<UserConfigData>, ProviderError> {
    let config_path = directory.join(USER_CONFIG_FILE_NAME);
    match std::fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(user_config_error(
                "user provider config directory is not a directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(user_config_error(
                "user provider config directory could not be inspected",
            ));
        }
    }
    if !path_exists_or_missing(&config_path, "user provider config could not be inspected")? {
        return Ok(None);
    }
    let mut config_file = open_user_config_file(&config_path, false)
        .map_err(|_| user_config_error("user provider config could not be opened"))?;
    let mut config_text = String::new();
    config_file
        .read_to_string(&mut config_text)
        .map_err(|_| user_config_error("user provider config could not be read"))?;
    let config: UserConfigFile = serde_json::from_str(&config_text)
        .map_err(|_| user_config_error("user provider config is invalid JSON"))?;
    if config.version != 1 {
        return Err(user_config_error(
            "unsupported user provider config version",
        ));
    }
    let auth_path = directory.join(USER_AUTH_FILE_NAME);
    let auth =
        if path_exists_or_missing(&auth_path, "user provider auth path could not be inspected")? {
            read_private_auth_file(&auth_path)?
        } else {
            UserAuthFile::default()
        };
    Ok(Some(UserConfigData { config, auth }))
}

pub(crate) fn path_exists_or_missing(path: &Path, message: &str) -> Result<bool, ProviderError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(user_config_error(message)),
    }
}
