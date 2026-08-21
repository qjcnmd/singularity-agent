//! User-level configuration, authentication and catalog seam.
//!
//! User config and auth remain one lifecycle: read, validate, and atomically
//! publish through the parent module's single source of truth.

pub(crate) mod auth;
pub(crate) mod catalog;
pub(crate) mod import;

pub(crate) use auth::*;
#[cfg(test)]
pub(crate) use catalog::load_models_cache;
pub use catalog::{
    ModelCacheStatus, ModelDiscoveryStatus, UserModelCatalog, UserModelCatalogEntry,
    UserProviderModelCatalog, read_user_model_catalog,
};
#[cfg(test)]
pub(crate) use import::parse_import_model_selector;
pub use import::{UserConfigImportResult, import_env_to_user_config};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::filesystem::{BoundedTextError, read_bounded_text_from_file};
use crate::config::schema::{ModelsFileReasoningVariant, deserialize_unique_map};
use crate::config::{ProviderConfigLayer, parse_model_selector};
use crate::error::ProviderError;
use crate::provider::contract::ProviderCapabilityDeclaration;
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
    #[serde(default)]
    pub(crate) auth_generation: Option<String>,
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
    #[serde(default)]
    pub(crate) capabilities: Option<ProviderCapabilityDeclaration>,
}

pub(crate) fn default_user_config_version() -> u32 {
    1
}

pub(crate) fn user_config_error(message: impl Into<String>) -> ProviderError {
    super::configuration_error(message, "provider_configuration_invalid")
}

#[derive(Clone)]
pub(crate) struct UserConfigData {
    pub(crate) directory: PathBuf,
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
        ensure_no_reparse_components(&home, true)?;
        Ok(Some(home))
    } else {
        let directory = home.join(USER_CONFIG_DIR_NAME);
        ensure_no_reparse_components(&directory, true)?;
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
    let repo = repository_boundary_root(&cwd)?;
    ensure_home_outside_root(path, &repo)
}

pub(crate) fn repository_boundary_root(cwd: &Path) -> Result<PathBuf, ProviderError> {
    let cwd = normalize_absolute_path(cwd)?;
    let mut current = cwd.clone();
    loop {
        let marker = current.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                return canonicalize_existing_prefix(&current);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(user_config_error(
                    "repository marker could not be inspected",
                ));
            }
        }
        if !current.pop() {
            break;
        }
    }
    canonicalize_existing_prefix(&cwd)
}

pub(crate) fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, ProviderError> {
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    user_config_error("user config path could not be canonicalized")
                })?;
                missing.push(component.to_os_string());
                if !current.pop() {
                    return Err(user_config_error(
                        "user config path could not be canonicalized",
                    ));
                }
            }
            Err(_) => {
                return Err(user_config_error(
                    "user config path could not be canonicalized",
                ));
            }
        }
    }
}

pub(crate) fn ensure_home_outside_root(path: &Path, root: &Path) -> Result<(), ProviderError> {
    let canonical_home = canonicalize_existing_prefix(path)?;
    let canonical_root = canonicalize_existing_prefix(root)?;
    if path_starts_with(&canonical_home, &canonical_root) {
        return Err(user_config_error(
            "SINGULARITY_HOME must not be inside the current repository",
        ));
    }
    Ok(())
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path_components = path.components();
        for prefix_component in prefix.components() {
            let Some(path_component) = path_components.next() else {
                return false;
            };
            if !path_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&prefix_component.as_os_str().to_string_lossy())
            {
                return false;
            }
        }
        true
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

pub(crate) fn ensure_no_reparse_components(
    path: &Path,
    allow_missing_tail: bool,
) -> Result<(), ProviderError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        #[cfg(windows)]
        if matches!(
            component,
            std::path::Component::Prefix(_) | std::path::Component::RootDir
        ) {
            continue;
        }
        let _metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing_tail && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(_) => {
                return Err(user_config_error(
                    "user provider config path component could not be checked",
                ));
            }
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if _metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(user_config_error(
                    "user provider config path contains a reparse point",
                ));
            }
        }
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
    ensure_no_reparse_components(&directory, false)?;
    if !path_exists_or_missing(&config_path, "user provider config could not be inspected")? {
        return Ok(None);
    }
    ensure_no_reparse_components(&config_path, false)?;
    let mut config_file = open_user_config_file(&config_path, false)
        .map_err(|_| user_config_error("user provider config could not be opened"))?;
    let config_text =
        read_bounded_text_from_file(&mut config_file, crate::MAX_DISCOVERY_RESPONSE_BYTES)
            .map_err(|error| match error {
                BoundedTextError::TooLarge => {
                    user_config_error("user provider config exceeds the size limit")
                }
                BoundedTextError::Read(_) => {
                    user_config_error("user provider config could not be read")
                }
            })?;
    let config: UserConfigFile = serde_json::from_str(&config_text)
        .map_err(|_| user_config_error("user provider config is invalid JSON"))?;
    if config.version != 1 {
        return Err(user_config_error(
            "unsupported user provider config version",
        ));
    }
    let auth = if let Some(generation) = config.auth_generation.as_deref() {
        let auth_path = auth_generation_path(&directory, generation)?;
        read_private_auth_file(&auth_path)?
    } else {
        UserAuthFile {
            schema_version: crate::USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    };
    Ok(Some(UserConfigData {
        directory,
        config,
        auth,
    }))
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
