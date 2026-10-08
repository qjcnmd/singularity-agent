//! 用户鉴权文件与它的安全保护（auth.json 只读访问）。
//!
//! 凭据目录里只有 auth.json 这一个文件，访问保护依靠 Windows 用户目录自身的 ACL。写入先落
//! 临时文件、再在同卷内原子改名，运行时不扫描其他凭据文件。

use std::collections::BTreeMap;
use std::path::Path;

use super::user_config_error;
use crate::error::ProviderError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserAuthFile {
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, UserAuthProvider>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserAuthProvider {
    pub(crate) api_key: String,
}

/// 只读 auth.json；文件不存在时得到一份空的默认凭据。
pub(crate) fn read_user_auth_file(directory: &Path) -> Result<UserAuthFile, ProviderError> {
    let path = directory.join(crate::USER_AUTH_FILE_NAME);
    let Some(text) = super::read_optional_config_text(&path)? else {
        return Ok(UserAuthFile::default());
    };
    let auth: UserAuthFile = serde_json::from_str(&text).map_err(|error| {
        user_config_error(format!(
            "user provider auth is invalid JSON ({:?}) at line {}, column {} in {}",
            error.classify(),
            error.line(),
            error.column(),
            path.display()
        ))
    })?;
    Ok(auth)
}
