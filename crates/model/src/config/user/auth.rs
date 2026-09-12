//! 用户鉴权文件与安全保护（auth.json 只读访问）。
//!
//! 凭据目录里只有一个 auth.json，读侧只认这一个文件名；
//! 访问保护依赖 Windows 用户目录自身的 ACL。
//!
//! 导入始终以临时文件加同卷原子改名更新唯一文件；运行时不扫描其他凭据文件。

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;

use super::user_config_error;
use crate::USER_AUTH_SCHEMA_VERSION;
use crate::config::schema::deserialize_unique_map;
use crate::error::ProviderError;
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
    let mut file = open_user_config_file(path)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|error| {
        user_config_error(format!("could not read {}: {error}", path.display()))
    })?;
    let auth: UserAuthFile = serde_json::from_str(&text).map_err(|error| {
        user_config_error(format!(
            "user provider auth is invalid JSON ({:?}) at line {}, column {} in {}",
            error.classify(),
            error.line(),
            error.column(),
            path.display()
        ))
    })?;
    if auth.schema_version != USER_AUTH_SCHEMA_VERSION {
        return Err(user_config_error("unsupported user provider auth version"));
    }
    Ok(auth)
}

pub(crate) fn open_user_config_file(path: &Path) -> Result<std::fs::File, ProviderError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .access_mode(FILE_GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(path).map_err(|error| {
        user_config_error(format!("could not open {}: {error}", path.display()))
    })?;
    Ok(file)
}
