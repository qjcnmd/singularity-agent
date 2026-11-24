use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::configuration_error;
use crate::USER_CONFIG_FILE_NAME;
use crate::error::ProviderError;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_model: Option<protocol::ModelSelection>,
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, UserConfigProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) web_search: Option<UserConfigWebSearch>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigWebSearch {
    #[serde(default)]
    pub(crate) base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) api_key: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigProvider {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) api_key: String,
    pub(crate) api_protocol: protocol::ProviderApiProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    pub(crate) base_url: String,
    #[serde(default)]
    pub(crate) models: BTreeMap<String, UserConfigModel>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigModel {
    pub(crate) max_context_tokens: u32,
    pub(crate) max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_variants: Option<Vec<String>>,
}

pub(crate) fn read_user_config_file(directory: &Path) -> Result<Option<UserConfigFile>, ProviderError> {
    let path = directory.join(USER_CONFIG_FILE_NAME);
    let config_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(configuration_error(format!("无法读取模型配置 {}：{error}", path.display())));
        }
    };
    let config_text = config_text.strip_prefix('\u{feff}').unwrap_or(&config_text);
    let config: UserConfigFile = serde_json::from_str(config_text).map_err(|error| {
        let reason = match error.classify() {
            serde_json::error::Category::Io => "读取失败",
            serde_json::error::Category::Syntax => "JSON 语法错误",
            serde_json::error::Category::Data => "配置字段无效",
            serde_json::error::Category::Eof => "内容不完整",
        };
        configuration_error(format!(
            "模型配置无效（{reason}）：{}，第 {} 行，第 {} 列",
            path.display(),
            error.line(),
            error.column()
        ))
    })?;
    Ok(Some(config))
}

pub(crate) fn write_user_config_file(directory: &Path, config: &UserConfigFile) -> Result<(), ProviderError> {
    let path = directory.join(USER_CONFIG_FILE_NAME);
    app_core::write_json_atomic(&path, config)
        .map_err(|error| configuration_error(format!("无法保存模型配置 {}：{error}", path.display())))
}
