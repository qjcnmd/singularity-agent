/// 用户级配置与认证接缝：`config.json` 与 `auth.json` 各自读取、校验，
/// 单文件操作只访问自己使用的文件；需要完整快照的操作才把两者组合起来。
///
/// 两个文件的公共读取方式（打开与共享模式）由本模块统一持有；auth 只处理
/// 凭据文件本身，不再对外提供通用文件打开细节。
pub(crate) mod auth;

pub(crate) use auth::*;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::schema::ModelsFileReasoningVariant;
use crate::error::ProviderError;
use crate::{USER_AUTH_FILE_NAME, USER_CONFIG_FILE_NAME};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigFile {
    #[serde(default = "default_user_config_version")]
    pub(crate) version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_model: Option<String>,
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, UserConfigProvider>,
}

impl Default for UserConfigFile {
    fn default() -> Self {
        Self {
            version: default_user_config_version(),
            default_model: None,
            providers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    pub(crate) base_url: String,
    #[serde(default)]
    pub(crate) models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    /// 空表代表「未声明变体」，与显式空表不可区分；不写回空对象。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) reasoning_variants: BTreeMap<String, ModelsFileReasoningVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supports_tool_choice: Option<bool>,
    /// 缺省即「不需要」；未声明时不写回，避免保存动作给无关模型补出字段。
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) requires_assistant_content_for_tool_calls: bool,
    /// Chat 端点的输出上限 wire 字段：值就是要发送的 JSON 字段名，缺省
    /// `max_tokens`。官方推理模型写 `max_completion_tokens`，端点用别的名字
    /// 时照写；Responses 不使用该字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_output_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_wire_format: Option<String>,
}

/// `skip_serializing_if` 谓词固定接收 `&bool`；缺省值即不写回。
#[allow(clippy::trivially_copy_pass_by_ref)] // serde 谓词签名要求按引用接收
fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn default_user_config_version() -> u32 {
    1
}

pub(crate) fn user_config_error(message: impl Into<String>) -> ProviderError {
    super::configuration_error(message, crate::error::PROVIDER_CONFIGURATION_INVALID_CODE)
}

#[derive(Clone)]
pub(crate) struct UserConfigData {
    pub(crate) config: UserConfigFile,
    pub(crate) auth: UserAuthFile,
}

/// 完整快照：配置与密钥各读各自的文件再组合。只有确实需要两份数据的操作
/// （快照捕获、提供方删除）走这里；单文件操作直接读它实际使用的那一份。
pub(crate) fn read_user_config_data_from_directory(
    directory: &Path,
) -> Result<Option<UserConfigData>, ProviderError> {
    let Some(config) = read_user_config_file(directory)? else {
        return Ok(None);
    };
    let auth = read_user_auth_file(directory)?;
    Ok(Some(UserConfigData { config, auth }))
}

/// 只读取 config.json；文件缺失时为 None，不依赖密钥文件。
pub(crate) fn read_user_config_file(
    directory: &Path,
) -> Result<Option<UserConfigFile>, ProviderError> {
    let path = directory.join(USER_CONFIG_FILE_NAME);
    let Some(config_text) = read_optional_config_text(&path)? else {
        return Ok(None);
    };
    let config: UserConfigFile = serde_json::from_str(&config_text).map_err(|error| {
        user_config_error(format!("invalid JSON in {}: {error}", path.display()))
    })?;
    if config.version != default_user_config_version() {
        return Err(user_config_error(
            "unsupported user provider config version",
        ));
    }
    Ok(Some(config))
}

/// 只读取 auth.json；文件缺失时得到默认空凭据，不由另一个文件决定。
/// 密钥是否存在只依据 auth 文件本身。
pub(crate) fn read_user_auth_file(directory: &Path) -> Result<UserAuthFile, ProviderError> {
    read_private_auth_file(&directory.join(USER_AUTH_FILE_NAME))
}

/// 两个配置文件共用的可选读取：一次打开即区分「文件不存在」与「读不出来」。
/// 打开本身就能给出这一结论，因此不再先做存在性探测——探测既不能提供原子性
/// （探测通过后文件仍可能消失），也不会改变错误分类。
pub(super) fn read_optional_config_text(path: &Path) -> Result<Option<String>, ProviderError> {
    // 两个配置文件共用同一份 Windows access/share 语义：允许其他写者共享读取。
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
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(user_config_error(format!(
                "could not open {}: {error}",
                path.display()
            )));
        }
    };
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|error| {
        user_config_error(format!("could not read {}: {error}", path.display()))
    })?;
    Ok(Some(text))
}
