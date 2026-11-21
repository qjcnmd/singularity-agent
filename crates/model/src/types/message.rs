use super::{ModelToolCall, ProviderBlockMetadata, ProviderMessageMetadata};

/// 消息体内部的结构化内容块；一条消息的内容就是若干块组成的有序列表。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    /// 图片文件保存在 `sessions/images/<会话 ID>/` 下，消息里只记录图片 ID、名称、类型和尺寸。
    Image(protocol::ImageAttachment),
    /// 思考正文只保存一次，签名和加密内容跟随对应块。
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    ToolCall(ModelToolCall),
}

/// 核心会话消息结构：以角色为标签的枚举，仅包含模型对话协议所需字段；
/// 本地展示元数据由会话日志外层单独保存，不混入模型消息。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum Message {
    /// Harness 和技能指令，包括请求前缀、轮换接手提示和末尾的整理要求。
    Developer {
        content: Vec<ContentBlock>,
    },
    /// 后台任务的状态通知，不显示为用户气泡。
    Notification {
        content: Vec<ContentBlock>,
    },
    User {
        content: Vec<ContentBlock>,
    },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// 提供方专用的推理续接信息（Messages / Responses 的来源标识），仅在后续请求发往同一提供方、模型、协议和地址时附带。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMessageMetadata>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID；名称和参数由原始 ToolCall 记录提供，这里不重复。
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

    /// 消息里的图片；历史显示、预览和模型输入都从这里读取。
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
    /// 构造纯文本用户消息或总结要求。
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { content: vec![ContentBlock::text(text)] }
    }
    /// 构造 Harness 或技能目录指令。
    pub fn developer(text: impl Into<String>) -> Self {
        Self::Developer { content: vec![ContentBlock::text(text)] }
    }
    /// Provider 解码和中断收尾共用的助手消息；正文与思考按同一顺序保存。
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
