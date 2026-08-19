//! User-level configuration, authentication and catalog seam.
//!
//! User config and auth remain one lifecycle: read, validate, and atomically
//! publish through the parent module's single source of truth.

use super::*;

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserConfigFile {
    #[serde(default = "default_user_config_version")]
    pub(super) version: u32,
    #[serde(default)]
    pub(super) default_provider: Option<String>,
    #[serde(default)]
    pub(super) default_model: Option<String>,
    #[serde(default)]
    pub(super) auth_generation: Option<String>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(super) providers: BTreeMap<String, UserConfigProvider>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserConfigProvider {
    pub(super) base_url: String,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(super) models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserConfigModel {
    #[serde(default)]
    pub(super) api_protocol: Option<String>,
    #[serde(default)]
    pub(super) max_context_tokens: Option<u32>,
    #[serde(default)]
    pub(super) max_output_tokens: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(super) reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    #[serde(default)]
    pub(super) default_variant: Option<String>,
    #[serde(default)]
    pub(super) tool_reasoning_history: Option<String>,
    #[serde(default)]
    pub(super) supports_developer_role: Option<bool>,
    #[serde(default)]
    pub(super) supports_tool_choice: Option<bool>,
    #[serde(default)]
    pub(super) requires_reasoning_content_for_tool_calls: bool,
    #[serde(default)]
    pub(super) requires_assistant_content_for_tool_calls: bool,
    #[serde(default)]
    pub(super) thinking_wire_format: Option<String>,
    #[serde(default)]
    pub(super) capabilities: Option<ProviderCapabilityDeclaration>,
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserAuthFile {
    #[serde(default = "default_auth_schema_version")]
    pub(super) schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(super) providers: BTreeMap<String, UserAuthProvider>,
}

impl fmt::Debug for UserAuthFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserAuthFile")
            .field("schema_version", &self.schema_version)
            .field(
                "providers",
                &self
                    .providers
                    .keys()
                    .map(|name| format!("{name}: [redacted]"))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

pub(super) fn default_user_config_version() -> u32 {
    1
}

pub(super) fn default_auth_schema_version() -> u32 {
    USER_AUTH_SCHEMA_VERSION
}

pub(super) fn deserialize_unique_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    let mut seen = std::collections::BTreeSet::new();
    if values.iter().any(|value| !seen.insert(value)) {
        return Err(de::Error::custom("duplicate model id"));
    }
    Ok(values)
}

pub(super) fn user_config_error(message: impl Into<String>) -> ProviderError {
    super::configuration_error(message, "provider_configuration_invalid")
}

impl Default for UserAuthFile {
    fn default() -> Self {
        Self {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserAuthProvider {
    pub(super) api_key: String,
}

impl fmt::Debug for UserAuthProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserAuthProvider")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct UserConfigData {
    pub(super) directory: PathBuf,
    pub(super) config: UserConfigFile,
    pub(super) auth: UserAuthFile,
}

#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserModelsCacheFile {
    pub(super) schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(super) providers: BTreeMap<String, UserModelsCacheRecord>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserModelsCacheRecord {
    pub(super) endpoint_sha256: String,
    pub(super) fetched_at_unix_seconds: u64,
    #[serde(deserialize_with = "deserialize_unique_vec")]
    pub(super) model_ids: Vec<String>,
}

/// Resolve the user-level directory shared by all worktrees.
fn user_config_directory_result() -> Result<Option<PathBuf>, ProviderError> {
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

pub(super) fn normalize_absolute_path(path: &Path) -> Result<PathBuf, ProviderError> {
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

pub(super) fn repository_boundary_root(cwd: &Path) -> Result<PathBuf, ProviderError> {
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

pub(super) fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, ProviderError> {
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

pub(super) fn ensure_home_outside_root(path: &Path, root: &Path) -> Result<(), ProviderError> {
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

pub(super) fn ensure_no_reparse_components(
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
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing_tail && error.kind() == std::io::ErrorKind::NotFound => {
                break;
            }
            Err(_) => {
                return Err(user_config_error(
                    "user config path components could not be inspected",
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(user_config_error(
                "user config path must not contain a symlink",
            ));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(user_config_error(
                    "user config path must not contain a reparse point",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn user_config_layer() -> Option<ProviderConfigLayer> {
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

pub(super) fn read_private_auth_file(path: &Path) -> Result<UserAuthFile, ProviderError> {
    let mut file = open_user_config_file(path, true)?;
    ensure_private_secret_handle(&file)?;
    let text = read_bounded_text_from_file(&mut file, crate::MAX_DISCOVERY_RESPONSE_BYTES)
        .map_err(|error| match error {
            BoundedTextError::TooLarge => {
                user_config_error("user provider auth exceeds the size limit")
            }
            BoundedTextError::Read(_) => user_config_error("user provider auth could not be read"),
        })?;
    let auth: UserAuthFile = serde_json::from_str(&text)
        .map_err(|_| user_config_error("user provider auth is invalid JSON"))?;
    if auth.schema_version != USER_AUTH_SCHEMA_VERSION {
        return Err(user_config_error("unsupported user provider auth version"));
    }
    Ok(auth)
}

#[cfg(all(test, unix))]
fn ensure_private_secret_file(path: &Path) -> Result<(), ProviderError> {
    let file = open_user_config_file(path, true)?;
    ensure_private_secret_handle(&file)
}

fn open_user_config_file(path: &Path, private: bool) -> Result<std::fs::File, ProviderError> {
    ensure_no_reparse_components(path, false)?;
    let parent = path
        .parent()
        .ok_or_else(|| user_config_error("user provider config path has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| user_config_error("user provider config path has no file name"))?;
    let directory = CapabilityDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|_| user_config_error("user provider auth could not be opened"))?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .access_mode(FILE_GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = directory
        .open_with(name, &options)
        .map_err(|_| user_config_error("user provider auth could not be opened"))?;
    let file = file.into_std();
    ensure_regular_user_config_handle(&file)?;
    if private {
        ensure_private_secret_handle(&file)?;
    }
    Ok(file)
}

fn ensure_regular_user_config_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file
        .metadata()
        .map_err(|_| user_config_error("user provider config metadata could not be checked"))?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "user provider config is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(user_config_error(
                "user provider config is not a regular file",
            ));
        }
    }
    Ok(())
}

pub(super) fn ensure_private_secret_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    ensure_regular_user_config_handle(file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file.metadata().map_err(|_| {
            user_config_error("user provider auth permissions could not be checked")
        })?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(user_config_error(
                "user provider auth file is not owner-only",
            ));
        }
    }
    Ok(())
}

pub(super) fn create_private_secret_file(path: &Path) -> Result<std::fs::File, ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| user_config_error("user provider config path has no parent"))?;
    ensure_no_reparse_components(parent, false)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|_| user_config_error("user provider auth file could not be created"))
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_identity {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[repr(C)]
    struct FileTime {
        _low_date_time: u32,
        _high_date_time: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        _volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        number_of_links: u32,
        _file_index_high: u32,
        _file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: RawHandle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    pub(super) fn read(file: &File) -> io::Result<(u32, u32)> {
        let mut information = MaybeUninit::<ByHandleFileInformation>::zeroed();
        // SAFETY: `file` owns a live Windows handle and `information` points to
        // writable storage of the exact C ABI layout required by the API.
        let result = unsafe {
            get_file_information_by_handle(file.as_raw_handle(), information.as_mut_ptr())
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Windows initialized the complete structure when the call
        // returned nonzero.
        let information = unsafe { information.assume_init() };
        Ok((information.file_attributes, information.number_of_links))
    }
}

fn read_user_config_data() -> Result<Option<UserConfigData>, ProviderError> {
    let Some(directory) = user_config_directory_result()? else {
        return Ok(None);
    };
    read_user_config_data_from_directory(directory)
}

pub(super) fn read_user_config_data_from_directory(
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
        UserAuthFile::default()
    };
    Ok(Some(UserConfigData {
        directory,
        config,
        auth,
    }))
}

fn auth_generation_path(directory: &Path, generation: &str) -> Result<PathBuf, ProviderError> {
    if !generation.starts_with(USER_AUTH_GENERATION_PREFIX)
        || !generation.ends_with(".json")
        || generation.contains(['/', '\\', ':'])
        || generation
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(user_config_error(
            "user provider auth generation reference is invalid",
        ));
    }
    let path = directory.join(generation);
    ensure_no_reparse_components(directory, false)?;
    if path_exists_or_missing(&path, "user provider auth path could not be inspected")? {
        ensure_no_reparse_components(&path, false)?;
    }
    Ok(path)
}

pub(super) fn path_exists_or_missing(path: &Path, message: &str) -> Result<bool, ProviderError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(user_config_error(message)),
    }
}

fn endpoint_fingerprint(base_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    let identity = normalized_endpoint_identity(base_url).unwrap_or_else(|_| base_url.to_string());
    digest.update(identity.as_bytes());
    format!("{:x}", digest.finalize())
}

fn user_model_override_is_selectable(
    provider_name: &str,
    model_name: &str,
    model: &UserConfigModel,
) -> bool {
    configured_model_from_user_file(provider_name, model_name, model).is_ok()
}

fn unix_timestamp_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) struct ModelsCacheLoad {
    pub(super) cache: UserModelsCacheFile,
    pub(super) status: ModelCacheStatus,
}

pub(super) fn load_models_cache(path: &Path) -> ModelsCacheLoad {
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

pub(super) struct ConfigWriterLock {
    _file: std::fs::File,
}

pub(super) fn acquire_config_writer_lock(
    directory: &Path,
) -> Result<ConfigWriterLock, ProviderError> {
    ensure_no_reparse_components(directory, false)?;
    let path = directory.join(".config.lock");
    let file = match config_writer_lock_options(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_no_reparse_components(&path, false)?;
            config_writer_lock_options(false).open(&path).map_err(|_| {
                user_config_error("provider config writer lock could not be acquired")
            })?
        }
        Err(_) => {
            return Err(user_config_error(
                "provider config writer lock could not be acquired",
            ));
        }
    };
    ensure_config_writer_lock_identity(&file)?;
    #[cfg(unix)]
    ensure_private_lock_handle(&file)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(user_config_error(
                "another provider config import is in progress",
            ));
        }
        Err(std::fs::TryLockError::Error(_)) => {
            return Err(user_config_error(
                "provider config writer lock could not be acquired",
            ));
        }
    }
    ensure_private_lock_handle(&file)?;
    Ok(ConfigWriterLock { _file: file })
}

fn config_writer_lock_options(create_new: bool) -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(create_new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .access_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
            )
            .share_mode(
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
            )
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
}

fn ensure_config_writer_lock_identity(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file.metadata().map_err(|_| {
        user_config_error("provider config writer lock identity could not be checked")
    })?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "provider config writer lock is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(user_config_error(
                "provider config writer lock must not have multiple hard links",
            ));
        }
    }
    #[cfg(windows)]
    {
        let (file_attributes, number_of_links) =
            windows_file_identity::read(file).map_err(|_| {
                user_config_error("provider config writer lock identity could not be checked")
            })?;
        if file_attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(user_config_error(
                "provider config writer lock must not be a reparse point",
            ));
        }
        if number_of_links != 1 {
            return Err(user_config_error(
                "provider config writer lock must not have multiple hard links",
            ));
        }
    }
    #[cfg(not(any(unix, windows)))]
    return Err(user_config_error(
        "provider config writer lock identity is unsupported on this platform",
    ));
    Ok(())
}

fn ensure_private_lock_handle(file: &std::fs::File) -> Result<(), ProviderError> {
    let metadata = file.metadata().map_err(|_| {
        user_config_error("provider config writer lock permissions could not be checked")
    })?;
    if !metadata.is_file() {
        return Err(user_config_error(
            "provider config writer lock is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(user_config_error(
                "provider config writer lock is not owner-only",
            ));
        }
    }
    Ok(())
}

fn new_auth_generation_name() -> String {
    format!(
        "{}{}.json",
        USER_AUTH_GENERATION_PREFIX,
        Uuid::new_v4().simple()
    )
}

pub(super) fn write_new_auth_generation(
    directory: &Path,
    generation: &str,
    contents: &str,
) -> Result<PathBuf, ProviderError> {
    let path = auth_generation_path(directory, generation)?;
    let mut file = create_private_secret_file(&path)?;
    let created = true;
    let result = (|| {
        ensure_private_secret_handle(&file)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())
            .map_err(|_| user_config_error("user provider auth could not be written"))?;
        file.sync_all()
            .map_err(|_| user_config_error("user provider auth could not be synced"))?;
        ensure_private_secret_handle(&file)?;
        Ok(())
    })();
    drop(file);
    if result.is_err() && created {
        let _ = std::fs::remove_file(&path);
    }
    result.map(|()| path)
}

/// Import the current or explicitly named dotenv file into split user config.
/// The API key is read from the file and written only to a versioned auth
/// generation; it is never accepted as a function argument or serialized in
/// the catalog.
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
        auth: UserAuthFile::default(),
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

pub(super) fn parse_import_model_selector(
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
    reasoning_variant: Option<&str>,
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
        || configured_model_from_user_file(provider_name, model_name, model).is_err()
    {
        return false;
    }
    reasoning_variant.is_none_or(|variant| model.reasoning_variants.contains_key(variant))
}

fn validate_discovered_model_ids(model_ids: Vec<String>) -> Result<Vec<String>, ProviderError> {
    if model_ids.len() > crate::MAX_DISCOVERED_MODEL_IDS {
        return Err(configuration_error(
            "provider models response exceeded the model id safety limit",
            "provider_configuration_invalid",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for model_id in &model_ids {
        validate_model_id(model_id, "discovered model id")?;
        if !seen.insert(model_id) {
            return Err(configuration_error(
                "provider models response contained duplicate model ids",
                "provider_configuration_invalid",
            ));
        }
    }
    if model_ids.is_empty() {
        return Err(configuration_error(
            "provider models response did not contain model ids",
            "provider_configuration_invalid",
        ));
    }
    Ok(model_ids)
}

fn public_diagnostic(error: &ProviderError) -> String {
    error
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
            .filter(|id| validate_model_id(id, "model id").is_ok())
            .cloned()
            .collect::<Vec<_>>();
        if provider_file
            .models
            .keys()
            .any(|id| validate_model_id(id, "model id").is_err())
        {
            diagnostics.push("one or more model ids are invalid".to_string());
        }
        let selectable_ids = provider_file
            .models
            .iter()
            .filter_map(|(id, model)| {
                (validate_model_id(id, "model id").is_ok()
                    && base_url_valid
                    && auth_valid
                    && user_model_override_is_selectable(provider_name, id, model))
                .then_some(id.clone())
            })
            .collect::<std::collections::BTreeSet<_>>();
        if explicit_ids.iter().any(|id| {
            provider_file
                .models
                .get(id)
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
            .collect::<std::collections::BTreeSet<_>>();
        let explicit_set = explicit_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut ids = explicit_ids;
        ids.extend(discovered_ids);
        ids.sort();
        ids.dedup();
        provider_catalogs.push(UserProviderModelCatalog {
            provider_name: provider_name.clone(),
            base_url_present: !provider_file.base_url.is_empty(),
            api_key_present: api_key.is_some(),
            discovery,
            models: ids
                .into_iter()
                .map(|id| UserModelCatalogEntry {
                    discovered: discovered_set.contains(&id),
                    explicit: explicit_set.contains(&id),
                    selectable: selectable_ids.contains(&id),
                    max_context_tokens: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .and_then(|model| model.max_context_tokens),
                    reasoning_variants: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .map(|model| {
                            model
                                .reasoning_variants
                                .keys()
                                .filter(|variant| {
                                    validate_identifier(variant, "reasoning variant").is_ok()
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_variant: user_config
                        .config
                        .providers
                        .get(provider_name)
                        .and_then(|provider| provider.models.get(&id))
                        .and_then(|model| model.default_variant.clone())
                        .filter(|variant| {
                            validate_identifier(variant, "reasoning variant").is_ok()
                        }),
                    id,
                })
                .collect(),
            error: (!diagnostics.is_empty()).then(|| diagnostics.join("; ")),
        });
    }
    if cache_changed {
        match serde_json::to_string_pretty(&cache) {
            Ok(cache_text) => {
                if write_json_file(&cache_path, &cache_text, false).is_err() {
                    cache_status = ModelCacheStatus::WriteFailed;
                }
            }
            Err(_) => cache_status = ModelCacheStatus::WriteFailed,
        }
    }
    Ok(UserModelCatalog {
        default_selector: user_config
            .config
            .default_model
            .as_deref()
            .and_then(|selector| {
                parse_model_selector(selector)
                    .ok()
                    .map(|_| selector.to_string())
            }),
        cache_status,
        providers: provider_catalogs,
    })
}
