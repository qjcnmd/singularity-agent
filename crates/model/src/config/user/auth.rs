//! 用户鉴权文件与安全保护（`auth.v1.json` 读写与权限校验）。
//!
//! 凭据目录里只有一个 `auth.v1.json`：写入方先落临时文件再同卷原子改名，
//! 读侧只认这一个文件名，任何时刻磁盘上看到的都是完整 JSON。
//!
//! 历史世代文件处置指引：早期布局按每次导入写入 `auth.v1-<uuid>.json`
//! 并由 config.json 的 `auth_generation` 字段指向最新一代。该机制已移除；
//! 残留的世代文件与本文件 schema 相同，仅含 api_key 明文，可由主代理或
//! 用户在确认新文件就绪后直接归档删除，运行时不再读取它们。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::{ensure_no_reparse_components, user_config_error};
use crate::config::filesystem::{BoundedTextError, read_bounded_text_from_file};
use crate::config::schema::deserialize_unique_map;
use crate::error::ProviderError;
use crate::{USER_AUTH_FILE_NAME, USER_AUTH_SCHEMA_VERSION};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserAuthFile {
    #[serde(default = "default_auth_schema_version")]
    pub(crate) schema_version: u32,
    #[serde(default, deserialize_with = "deserialize_unique_map")]
    pub(crate) providers: BTreeMap<String, UserAuthProvider>,
}

impl Default for UserAuthFile {
    fn default() -> Self {
        Self {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::new(),
        }
    }
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

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserAuthProvider {
    pub(crate) api_key: String,
}

impl fmt::Debug for UserAuthProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserAuthProvider")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

pub(crate) fn default_auth_schema_version() -> u32 {
    USER_AUTH_SCHEMA_VERSION
}

pub(crate) fn read_private_auth_file(path: &Path) -> Result<UserAuthFile, ProviderError> {
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
pub(crate) fn ensure_private_secret_file(path: &Path) -> Result<(), ProviderError> {
    let file = open_user_config_file(path, true)?;
    ensure_private_secret_handle(&file)
}

pub(crate) fn open_user_config_file(
    path: &Path,
    private: bool,
) -> Result<std::fs::File, ProviderError> {
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

pub(crate) fn ensure_regular_user_config_handle(file: &std::fs::File) -> Result<(), ProviderError> {
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

pub(crate) fn ensure_private_secret_handle(file: &std::fs::File) -> Result<(), ProviderError> {
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

pub(crate) fn create_private_secret_file(path: &Path) -> Result<std::fs::File, ProviderError> {
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

/// 返回凭据目录下唯一的 `auth.v1.json` 路径；目录边界先经 reparse 校验。
pub(crate) fn user_auth_file_path(directory: &Path) -> Result<PathBuf, ProviderError> {
    ensure_no_reparse_components(directory, false)?;
    Ok(directory.join(USER_AUTH_FILE_NAME))
}

pub(crate) struct ConfigWriterLock {
    pub(crate) _file: std::fs::File,
}

pub(crate) fn acquire_config_writer_lock(
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
