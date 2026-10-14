use crate::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub(crate) const DEFAULT_CHAT_REASONING_FIELD: &str = "reasoning_content";

pub(crate) const CHAT_REASONING_FIELDS: &[&str] =
    &[DEFAULT_CHAT_REASONING_FIELD, "reasoning", "reasoning_text"];

/// 提供方私有的推理状态：在适配器边界内可以安全重放，不展示，也不进入公开会话、trace、
/// 评估或错误结构。类型公开是因为 harness 持有轮次之间的推理重放边界。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        /// 实际请求地址；地址变化后不复用这份私有数据。
        endpoint: String,
        reasoning_content: String,
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
        /// 提供方的完整输出序列，逐字保留；适配器只往后追加 function_call_output 项。
        items: Vec<Value>,
    },
}

impl fmt::Debug for ProviderReasoningReplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ProviderReasoningReplay");
        match self {
            Self::Chat { reasoning_content, .. } => {
                debug.field("protocol", &"chat").field("reasoning_content_len", &reasoning_content.len());
            }
            Self::Responses { items, .. } => {
                debug
                    .field("protocol", &"responses")
                    .field("output_item_count", &items.len())
                    .field("reasoning_item_present", &true);
            }
        }
        debug.finish()
    }
}

impl ProviderReasoningReplay {
    /// 续接数据只在原提供方、模型、协议和请求地址下复用；档位不影响数据身份。
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
