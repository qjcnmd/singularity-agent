use crate::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub(crate) const DEFAULT_CHAT_REASONING_FIELD: &str = "reasoning_content";

pub(crate) const CHAT_REASONING_FIELDS: &[&str] =
    &[DEFAULT_CHAT_REASONING_FIELD, "reasoning", "reasoning_text"];

/// 随助手消息保存在会话日志中的提供方续接信息，用于后续请求向同一模型回传思考字段；转换前端历史条目时不展示。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum ProviderMessageMetadata {
    Chat {
        provider_name: String,
        model_name: String,
        /// 实际请求地址；地址变化后不复用这份私有数据。
        endpoint: String,
        /// 保留提供方返回时用的字段名。
        reasoning_field: String,
        /// OpenAI 兼容端点返回的结构化签名或加密推理；不当作文本重建。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_details: Vec<Value>,
    },
    Responses {
        provider_name: String,
        model_name: String,
        endpoint: String,
    },
}

impl fmt::Debug for ProviderMessageMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ProviderMessageMetadata");
        debug.field(
            "protocol",
            &match self {
                Self::Chat { .. } => "chat",
                Self::Responses { .. } => "responses",
            },
        );
        debug.finish()
    }
}

impl ProviderMessageMetadata {
    /// 续接数据只在提供方、模型、协议和请求地址都相同时复用；思考档位不同不影响。
    pub(crate) fn is_for_model(
        &self,
        provider_name: &str,
        model_name: &str,
        protocol: ProviderApiProtocol,
        endpoint: &str,
    ) -> bool {
        let (provider, model, source_endpoint) = self.model_identity();
        provider == provider_name
            && model == model_name
            && source_endpoint == endpoint
            && matches!(
                (self, protocol),
                (Self::Chat { .. }, ProviderApiProtocol::Chat)
                    | (Self::Responses { .. }, ProviderApiProtocol::Responses)
            )
    }

    fn model_identity(&self) -> (&str, &str, &str) {
        match self {
            Self::Chat { provider_name, model_name, endpoint, .. }
            | Self::Responses { provider_name, model_name, endpoint, .. } => {
                (provider_name, model_name, endpoint)
            }
        }
    }
}

/// 原始输出中除正文、思考和工具参数之外的续接字段，不进入公开事件。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderBlockMetadata(pub serde_json::Map<String, Value>);
impl fmt::Debug for ProviderBlockMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderBlockMetadata([redacted])")
    }
}
