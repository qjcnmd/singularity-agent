use super::{ModelToolCall, ProviderBlockMetadata, ProviderMessageMetadata};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn provider_metadata(&self) -> Option<&ProviderMessageMetadata> {
        match self {
            Self::Assistant { provider_metadata, .. } => provider_metadata.as_ref(),
            _ => None,
        }
    }
}

impl Message {
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

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), metadata: None }
    }
}
