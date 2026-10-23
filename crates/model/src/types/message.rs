use super::{ModelToolCall, ProviderBlockMetadata, ProviderMessageMetadata};
/// 模型协议编码时使用的角色，通知在边界映射为 user。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelRole {
    Developer,
    User,
    Assistant,
    Tool,
}

/// 消息体内部的结构化内容块；一条消息的内容就是若干块组成的有序列表。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    /// 图片像素保存在会话所属目录，消息只保留不可变快照描述。
    Image(singularity_protocol::ImageAttachment),
    /// 思考正文只保存一次，签名和加密内容跟随对应块。
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ProviderBlockMetadata>,
    },
    ToolCall(ModelToolCall),
}

/// 核心会话消息结构：以角色为标签的枚举，每个角色只携带自己合法的字段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum Message {
    /// Harness 和技能指令，只在请求前缀中使用。
    Developer { content: Vec<ContentBlock> },
    /// 后台任务的状态通知，不显示为用户气泡。
    Notification { content: Vec<ContentBlock> },
    #[serde(rename_all = "camelCase")]
    User {
        content: Vec<ContentBlock>,
        /// 技能引用展开前的用户文字；模型消费 content，界面保持原始显示。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// 模型提供方的私有推理状态，用于 Responses 这类协议重放推理的连续性。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMessageMetadata>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID；名称和参数由原始 ToolCall 记录提供，这里不重复。
        tool_call_id: String,
        is_error: bool,
        /// 观测到的工具执行耗时；结果未知或没有执行时缺省。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// read 实际读取到的源文件范围；只有 read 的成功结果才带这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_source: Option<singularity_protocol::ReadSource>,
    },
}

impl Message {
    /// 按保存顺序读取消息内容。
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::Developer { content }
            | Self::Notification { content }
            | Self::User { content, .. }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    /// 用户消息和任务标题共用展示文字；借用原文，模型仍消费 content。
    pub fn text_for_display<'a>(&'a self, index: usize, text: &'a str) -> &'a str {
        match self {
            Self::User { display_text: Some(display), .. } if index == 0 => display,
            _ => text,
        }
    }

    /// 消息的正文，不含思考内容。
    pub fn content_text(&self) -> String {
        content_text(self.content())
    }

    /// 消息里的图片；历史显示、预览和模型输入都从这里读取。
    pub fn images(&self) -> impl Iterator<Item = &singularity_protocol::ImageAttachment> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
    }

    /// 消息里的工具调用载荷。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ModelToolCall> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 工具结果对应的调用 ID；其他角色为 None。
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }

    /// provider 的推理重放数据；只有 assistant 消息会带。
    pub fn provider_metadata(&self) -> Option<&ProviderMessageMetadata> {
        match self {
            Self::Assistant { provider_metadata, .. } => provider_metadata.as_ref(),
            _ => None,
        }
    }
}

impl Message {
    /// 构造纯文本用户消息或总结要求。
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentBlock::text(text)],
            display_text: None,
        }
    }
    /// 构造 Harness 或技能目录指令。
    pub fn developer(text: impl Into<String>) -> Self {
        Self::Developer { content: vec![ContentBlock::text(text)] }
    }
    /// 编码时通知按用户消息发送，工具结果带上对应的调用 ID。
    pub(crate) fn role(&self) -> ModelRole {
        match self {
            Self::Developer { .. } => ModelRole::Developer,
            Self::User { .. } | Self::Notification { .. } => ModelRole::User,
            Self::Assistant { .. } => ModelRole::Assistant,
            Self::ToolResult { .. } => ModelRole::Tool,
        }
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

/// 拼接模型可见的文本块。
fn content_text(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text { text: part, .. } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    text
}

impl ContentBlock {
    /// 构造不含提供方续接字段的文本块。
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), metadata: None }
    }
}
