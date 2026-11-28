use std::fmt;

pub use protocol::ModelUsage;
use protocol::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProviderError;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

pub fn estimate_tool_tokens<'a>(tools: impl IntoIterator<Item = &'a ToolDefinition>) -> u64 {
    let tools: Vec<_> = tools.into_iter().collect();
    if tools.is_empty() {
        return 0;
    }
    app_core::estimate_tokens_of(&serde_json::to_string(&tools).expect("tool definitions are serializable"))
        + 4
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderMessageMetadata {
    pub protocol: ProviderApiProtocol,
    pub provider_name: String,
    pub model_name: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_digest: Option<String>,
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

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderBlockMetadata(pub(crate) serde_json::Map<String, Value>);

impl fmt::Debug for ProviderBlockMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderBlockMetadata([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolCall {
    #[serde(rename = "id")]
    pub tool_call_id: String,
    #[serde(rename = "name")]
    pub tool_name: String,
    #[serde(rename = "args")]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProviderBlockMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    Image(protocol::ImageAttachment),
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    ToolCall(ModelToolCall),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), metadata: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum Message {
    Developer {
        content: Vec<ContentBlock>,
    },
    Notification {
        content: Vec<ContentBlock>,
    },
    User {
        content: Vec<ContentBlock>,
    },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMessageMetadata>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        tool_call_id: String,
        is_error: bool,
    },
}

impl Message {
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::Developer { content }
            | Self::Notification { content }
            | Self::User { content }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    pub fn content_text(&self) -> String {
        content_text(self.content())
    }

    pub fn images(&self) -> impl Iterator<Item = &protocol::ImageAttachment> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ModelToolCall> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    pub(crate) fn provider_metadata(&self) -> Option<&ProviderMessageMetadata> {
        match self {
            Self::Assistant { provider_metadata, .. } => provider_metadata.as_ref(),
            _ => None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User { content: vec![ContentBlock::text(text)] }
    }

    pub fn developer(text: impl Into<String>) -> Self {
        Self::Developer { content: vec![ContentBlock::text(text)] }
    }

    pub fn assistant(
        text: String,
        thinking: String,
        calls: Vec<ModelToolCall>,
        replay: Option<ProviderMessageMetadata>,
    ) -> Self {
        let mut content = Vec::new();
        if !thinking.is_empty() {
            content.push(ContentBlock::Thinking { thinking, metadata: None });
        }
        if !text.is_empty() {
            content.push(ContentBlock::Text { text, metadata: None });
        }
        content.extend(calls.into_iter().map(ContentBlock::ToolCall));
        Self::Assistant { content, provider_metadata: replay }
    }
}

fn content_text(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text { text: part, .. } = block
            && !part.is_empty()
        {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    text
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<Message>,
    pub images: std::collections::HashMap<String, String>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelStopReason {
    Stop,
    Length,
    Failed(ProviderError),
}

#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnResponse {
    pub assistant_message: Message,
    pub usage: Option<ModelUsage>,
    pub(crate) stop_reason: ModelStopReason,
    pub(crate) refusal: bool,
}

impl ModelTurnResponse {
    pub(crate) fn failed(error: ProviderError, assistant_message: Message) -> Self {
        Self {
            assistant_message,
            usage: None,
            stop_reason: ModelStopReason::Failed(error),
            refusal: false,
        }
    }

    pub fn error(&self) -> Option<&ProviderError> {
        match &self.stop_reason {
            ModelStopReason::Failed(error) => Some(error),
            ModelStopReason::Stop | ModelStopReason::Length => None,
        }
    }

    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == ModelStopReason::Length
    }

    pub fn is_refusal(&self) -> bool {
        self.refusal
    }
}
