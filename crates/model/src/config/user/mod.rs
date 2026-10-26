//! `config.json` 的数据结构与原子读写。

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
    pub(crate) default_model: Option<singularity_protocol::ModelSelection>,
    #[serde(default)]
    pub(crate) providers: BTreeMap<String, UserConfigProvider>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserConfigProvider {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) api_key: String,
    pub(crate) api_protocol: singularity_protocol::ProviderApiProtocol,
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
    /// 可选的推理档位列表；省略或为空时不提供推理档位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_variants: Option<Vec<String>>,
    /// Chat 请求里表示最大输出 Token 的字段名，默认是 `max_tokens`，也可设为 `max_completion_tokens`；Responses 不使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_output_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_wire_format: Option<String>,
}

/// 读取完整配置；文件不存在时返回 None。
pub(crate) fn read_user_config_file(directory: &Path) -> Result<Option<UserConfigFile>, ProviderError> {
    let path = directory.join(USER_CONFIG_FILE_NAME);
    let config_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(configuration_error(format!("无法读取模型配置 {}：{error}", path.display())));
        }
    };
    let config: UserConfigFile = serde_json::from_str(&config_text).map_err(|error| {
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

/// 序列化后原子替换整个配置：被删掉的提供方、模型和推理档位随保存消失。
/// 键按 `BTreeMap` 排序，与文件原先的书写顺序无关。
pub(crate) fn write_user_config_file(directory: &Path, config: &UserConfigFile) -> Result<(), ProviderError> {
    singularity_core::create_data_dir(directory).map_err(configuration_error)?;
    let path = directory.join(USER_CONFIG_FILE_NAME);
    singularity_core::write_json_atomic(&path, config)
        .map_err(|error| configuration_error(format!("无法保存模型配置 {}：{error}", path.display())))
}
