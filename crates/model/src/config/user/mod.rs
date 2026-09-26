/// 用户配置与认证的接缝：`config.json` 和 `auth.json` 各自读取、各自校验，只碰一个文件的操作
/// 就只访问那一个文件；两个文件共用的读取方式（打开与共享模式）统一放在本模块，auth 只管
/// 凭据文件本身，不对外暴露通用的文件打开细节。
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
    pub(crate) api_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    pub(crate) base_url: String,
    #[serde(default)]
    pub(crate) models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) automatic_fields: Option<Vec<singularity_protocol::ModelConfigurationField>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    /// 缺省可由发现结果补齐；显式空表保留用户不提供思考选项的选择。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_variants: Option<BTreeMap<String, ModelsFileReasoningVariant>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supports_developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supports_tool_choice: Option<bool>,
    /// 缺省就是「不需要」；未声明时保存不写回，免得给无关模型补出这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) requires_reasoning_content_for_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) requires_assistant_content_for_tool_calls: bool,
    /// Chat 端点输出上限所用的线上字段名，就是要发送的 JSON 字段名，缺省是 `max_tokens`；
    /// 官方推理模型写 `max_completion_tokens`，端点用别的名字就照它的写。Responses 不用它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_output_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_wire_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_modalities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) output_modalities: Option<Vec<String>>,
}

/// `skip_serializing_if` 谓词固定收 `&bool`；返回 true 时不写回。
#[allow(clippy::trivially_copy_pass_by_ref)] // serde 要求谓词签名按引用接收
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

/// 完整快照：配置和密钥各读各自的文件再合起来；只有确实需要两份数据的操作（快照捕获、
/// 删除提供方）才走这里。
pub(crate) fn read_user_config_data_from_directory(
    directory: &Path,
) -> Result<Option<UserConfigData>, ProviderError> {
    let Some(config) = read_user_config_file(directory)? else {
        return Ok(None);
    };
    let auth = read_user_auth_file(directory)?;
    Ok(Some(UserConfigData { config, auth }))
}

/// 只读 config.json；文件不存在时返回 None，不依赖密钥文件。
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

/// 只读 auth.json；文件不存在时得到一份空的默认凭据。密钥在不在只看 auth 文件本身。
pub(crate) fn read_user_auth_file(directory: &Path) -> Result<UserAuthFile, ProviderError> {
    read_private_auth_file(&directory.join(USER_AUTH_FILE_NAME))
}

/// 两个配置文件共用的可选读取：一次打开就能区分「文件不存在」和「读不出来」。不先探测文件
/// 是否存在：探测既保证不了原子性（探测通过后文件照样可能消失），也不会改变错误分类。
pub(super) fn read_optional_config_text(path: &Path) -> Result<Option<String>, ProviderError> {
    let mut file = match std::fs::File::open(path) {
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
