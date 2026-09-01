//! 用户鉴权文件与安全保护（`auth.json` 只读访问与权限校验）。
//!
//! 凭据目录里只有一个 `auth.json`，读侧只认这一个文件名，并对手柄复检
//! 常规文件与属主专用权限（Unix 0600；Windows 依赖用户目录自身的 ACL，
//! 不额外检查文件权限）；任何越界状态都 fail closed。
//!
//! 导入始终以临时文件加同卷原子改名更新唯一文件；运行时不扫描其他凭据文件。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::user_config_error;
use crate::config::filesystem::{BoundedTextError, read_bounded_text_from_file};
use crate::config::schema::deserialize_unique_map;
use crate::error::ProviderError;
use crate::{USER_AUTH_FILE_NAME, USER_AUTH_SCHEMA_VERSION};
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
    let text = read_bounded_text_from_file(&mut file, crate::MAX_CONFIG_AUTH_FILE_BYTES).map_err(
        |error| match error {
            BoundedTextError::TooLarge => {
                user_config_error("user provider auth exceeds the size limit")
            }
            BoundedTextError::Read => user_config_error("user provider auth could not be read"),
        },
    )?;
    let auth: UserAuthFile = serde_json::from_str(&text)
        .map_err(|_| user_config_error("user provider auth is invalid JSON"))?;
    if auth.schema_version != USER_AUTH_SCHEMA_VERSION {
        return Err(user_config_error("unsupported user provider auth version"));
    }
    Ok(auth)
}

pub(crate) fn open_user_config_file(
    path: &Path,
    private: bool,
) -> Result<std::fs::File, ProviderError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .access_mode(FILE_GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options
        .open(path)
        .map_err(|_| user_config_error("user provider auth could not be opened"))?;
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

/// 返回凭据目录下唯一的 `auth.json` 路径。
pub(crate) fn user_auth_file_path(directory: &Path) -> Result<PathBuf, ProviderError> {
    Ok(directory.join(USER_AUTH_FILE_NAME))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    #[test]
    fn oversized_auth_is_rejected_before_parsing() {
        let directory = tempfile::tempdir().expect("temporary user config directory");
        let path = directory.path().join(USER_AUTH_FILE_NAME);
        std::fs::write(&path, "x".repeat(crate::MAX_CONFIG_AUTH_FILE_BYTES + 1))
            .expect("write oversized auth file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict auth file to owner");
        }

        let error = read_private_auth_file(&path).expect_err("oversized auth must fail closed");
        assert_eq!(
            error.error.message,
            "user provider auth exceeds the size limit"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_auth_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary user config directory");
        let path = directory.path().join(USER_AUTH_FILE_NAME);
        std::fs::write(&path, r#"{"schema_version":1,"providers":{}}"#).expect("write auth file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("make auth file group-readable");

        let error = read_private_auth_file(&path).expect_err("shared auth file must fail closed");
        assert_eq!(
            error.error.message,
            "user provider auth file is not owner-only"
        );
    }
}
