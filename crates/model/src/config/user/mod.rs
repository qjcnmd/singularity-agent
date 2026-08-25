//! User-level configuration and authentication seam.
//!
//! User config and auth remain one lifecycle: read, validate, and expose to
//! the parent module's single source of truth.

pub(crate) mod auth;

pub(crate) use auth::*;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::filesystem::{BoundedTextError, read_bounded_text_from_file};
use crate::config::schema::{ModelsFileReasoningVariant, deserialize_unique_map};
use crate::config::{ProviderConfigLayer, parse_model_selector};
use crate::error::ProviderError;
use crate::{USER_CONFIG_DIR_NAME, USER_CONFIG_FILE_NAME};

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

/// Resolve the user-level directory shared by all worktrees.
pub(crate) fn user_config_directory_result() -> Result<Option<PathBuf>, ProviderError> {
    let explicit_home = std::env::var_os("SINGULARITY_HOME");
    let home = explicit_home
        .clone()
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"));
    let Some(home) = home else {
        return Ok(None);
    };
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Err(user_config_error(
            "SINGULARITY_HOME must be a non-empty absolute path",
        ));
    }
    let home = normalize_absolute_path(&home)?;
    if explicit_home.is_some() {
        ensure_home_not_repo_controlled(&home)?;
        ensure_no_reparse_point(&home, true)?;
        Ok(Some(home))
    } else {
        let directory = home.join(USER_CONFIG_DIR_NAME);
        ensure_no_reparse_point(&directory, true)?;
        Ok(Some(directory))
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

fn ensure_home_not_repo_controlled(path: &Path) -> Result<(), ProviderError> {
    let cwd = std::env::current_dir()
        .map_err(|_| user_config_error("current directory could not be read"))?;
    singularity_core::ensure_singularity_home_outside_workspace(path, &cwd)
        .map_err(user_config_error)
}

/// 检查路径本体不是 reparse point（Windows junction/symlink）。检查范围收缩
/// 到 `.singularity` 目录及其内文件：用户目录（如 Junction 化的 `%USERPROFILE%`
/// 或自定义 SINGULARITY_HOME）的祖先不再逐级校验，恢复 Junction 用户目录可用。
pub(crate) fn ensure_no_reparse_point(
    path: &Path,
    allow_missing_tail: bool,
) -> Result<(), ProviderError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing_tail && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(_) => {
            return Err(user_config_error(
                "user provider config path could not be checked",
            ));
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(user_config_error(
                "user provider config path is a reparse point",
            ));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
    }
    Ok(())
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
    match std::fs::symlink_metadata(&directory) {
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
    ensure_no_reparse_point(&directory, false)?;
    if !path_exists_or_missing(&config_path, "user provider config could not be inspected")? {
        return Ok(None);
    }
    ensure_no_reparse_point(&config_path, false)?;
    let mut config_file = open_user_config_file(&config_path, false)
        .map_err(|_| user_config_error("user provider config could not be opened"))?;
    let config_text = read_bounded_text_from_file(
        &mut config_file,
        crate::MAX_CONFIG_AUTH_FILE_BYTES,
    )
    .map_err(|error| match error {
        BoundedTextError::TooLarge => {
            user_config_error("user provider config exceeds the size limit")
        }
        BoundedTextError::Read => user_config_error("user provider config could not be read"),
    })?;
    let config: UserConfigFile = serde_json::from_str(&config_text)
        .map_err(|_| user_config_error("user provider config is invalid JSON"))?;
    if config.version != 1 {
        return Err(user_config_error(
            "unsupported user provider config version",
        ));
    }
    let auth_path = user_auth_file_path(&directory)?;
    let auth =
        if path_exists_or_missing(&auth_path, "user provider auth path could not be inspected")? {
            ensure_no_reparse_point(&auth_path, false)?;
            read_private_auth_file(&auth_path)?
        } else {
            UserAuthFile::default()
        };
    Ok(Some(UserConfigData { config, auth }))
}

pub(crate) fn path_exists_or_missing(path: &Path, message: &str) -> Result<bool, ProviderError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(user_config_error(message)),
    }
}

pub(crate) fn user_config_layer() -> Option<ProviderConfigLayer> {
    match read_user_config_data() {
        Ok(Some(user_config)) => {
            let mut layer = ProviderConfigLayer {
                user_config: Some(user_config.clone()),
                user_config_error: None,
                ..ProviderConfigLayer::default()
            };
            let default_provider = user_config
                .config
                .default_provider
                .clone()
                .or_else(|| {
                    user_config
                        .config
                        .default_model
                        .as_deref()
                        .and_then(|selector| parse_model_selector(selector).ok())
                        .map(|selector| selector.provider_name.to_string())
                })
                .or_else(|| user_config.config.providers.keys().next().cloned());
            if let Some(provider_name) = default_provider
                && let Some(provider) = user_config.config.providers.get(&provider_name)
            {
                layer.provider_name = Some(provider_name.clone());
                layer.base_url = Some(provider.base_url.clone());
                layer.api_key = user_config
                    .auth
                    .providers
                    .get(&provider_name)
                    .map(|provider| provider.api_key.clone());
                layer.model_name = user_config.config.default_model.clone();
            }
            Some(layer)
        }
        Ok(None) => None,
        Err(error) => Some(ProviderConfigLayer {
            user_config_error: Some(error),
            ..ProviderConfigLayer::default()
        }),
    }
}
