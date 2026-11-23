use protocol::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// 随助手消息保存在会话日志中的提供方续接信息，用于后续请求向同一模型回传思考签名或加密内容；转换前端历史条目时不展示。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessageMetadata {
    pub protocol: ProviderApiProtocol,
    pub provider_name: String,
    pub model_name: String,
    /// 实际请求地址；地址变化后不复用这份私有数据。
    pub endpoint: String,
    /// Messages 协议生成该回复时的请求前缀链式哈希；前缀变化时仅剥离过期的带签名思考块。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_digest: Option<String>,
    /// 生成该回复时用户选定的推理档位，供切模型预压恢复旧模型配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl fmt::Debug for ProviderMessageMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderMessageMetadata")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
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
        self.protocol == protocol
            && self.provider_name == provider_name
            && self.model_name == model_name
            && self.endpoint == endpoint
    }
}

/// 原始输出中除正文、思考和工具参数之外的续接字段，不进入公开事件。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderBlockMetadata(pub(crate) serde_json::Map<String, Value>);
impl fmt::Debug for ProviderBlockMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderBlockMetadata([redacted])")
    }
}
