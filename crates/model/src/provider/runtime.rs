use crate::{
    ProviderApiProtocol, ProviderCapabilityDeclaration, ProviderConfigSource,
    ProviderToolReasoningMode, ThinkingWireFormat,
};
use std::future::Future;
use std::sync::Arc;

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

/// One fully resolved catalog selection.  Keeping the canonical variant,
/// enabled state and the single wire effort together prevents a second runtime
/// mapping table from silently changing the provider request.
#[derive(Clone)]
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) reasoning_enabled: bool,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) tool_reasoning_mode: ProviderToolReasoningMode,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
    /// 合并后的用户显式能力声明；协议契约构造时叠加到静态基线。
    pub(crate) capability_overrides: Option<ProviderCapabilityDeclaration>,
}

/// Provider transport runtime ownership: an app-server borrows its existing handle, while
/// independent consumers own a dedicated runtime shared by provider clones.
#[derive(Clone)]
pub(crate) enum ProviderRuntime {
    External(tokio::runtime::Handle),
    Owned(Arc<tokio::runtime::Runtime>),
}

impl ProviderRuntime {
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        match self {
            Self::External(handle) => handle.block_on(future),
            Self::Owned(runtime) => runtime.block_on(future),
        }
    }
}
