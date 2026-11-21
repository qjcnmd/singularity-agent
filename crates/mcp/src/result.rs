//! MCP 公开结果、错误与工具发现类型。

use crate::client::Connection;
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};
use std::{fmt, sync::Arc};
use tokio_util::sync::CancellationToken;

/// MCP 配置、连接、发现与调用失败时返回的轻量错误。
#[must_use]
#[derive(Debug)]
pub struct McpError(pub(crate) String);

impl McpError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub(crate) fn not_found() -> Self {
        Self("MCP 服务器不存在。".into())
    }

    pub(crate) fn io(context: &str, source: std::io::Error) -> Self {
        Self(format!("{context}：{source}"))
    }
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for McpError {}

/// 本轮发现的一个 MCP 工具；调用时使用发现时的连接，本轮内不变。
#[derive(Clone)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    connection: Arc<Connection>,
}

impl fmt::Debug for McpTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters_schema", &self.parameters_schema)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    pub(crate) fn new(
        name: String,
        description: String,
        parameters_schema: serde_json::Value,
        connection: Arc<Connection>,
    ) -> Self {
        Self {
            name,
            description,
            parameters_schema,
            connection,
        }
    }

    /// 判断该工具是否无需传入任何参数，供延迟加载摘要标注 `(no args)`。
    pub fn has_no_args(&self) -> bool {
        let Some(object) = self.parameters_schema.as_object() else {
            return false;
        };
        let required_empty =
            object.get("required").and_then(serde_json::Value::as_array).is_none_or(Vec::is_empty);
        let props_empty = object
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .is_none_or(serde_json::Map::is_empty);
        let has_dynamic_or_composed = ["anyOf", "oneOf", "allOf", "patternProperties", "$ref"]
            .iter()
            .any(|key| object.contains_key(*key))
            || matches!(
                object.get("additionalProperties"),
                Some(value) if *value != serde_json::Value::Bool(false)
            );
        required_empty && props_empty && !has_dynamic_or_composed
    }

    /// 调用服务器原始工具；连接负责总超时和有界取消通知，调用不会自动重试。
    pub async fn call(
        &self,
        arguments: serde_json::Map<String, serde_json::Value>,
        signal: &CancellationToken,
    ) -> Result<McpToolResult, McpError> {
        let result = self.connection.call(&self.name, arguments, signal).await?;
        Ok(convert(result))
    }
}

/// 一台服务器本轮的发现结果；连不上时没有工具，失败原因在 `error`。
#[derive(Debug)]
pub struct McpServerDiscovery {
    pub id: String,
    pub tools: Vec<McpTool>,
    /// 要写进 Developer 指令的内容：服务器自带的说明，或连接失败的原因。
    pub prompt: Option<String>,
    pub error: Option<String>,
}

/// 一次工具发现的结果；`error` 是 mcp.json 读不出来的原因，此时没有服务器。
#[derive(Debug, Default)]
pub struct McpDiscovery {
    pub servers: Vec<McpServerDiscovery>,
    pub error: Option<String>,
}

/// 工具结果；图片保留服务器返回的 Base64 内容，由 Agent 检查和保存。
#[derive(Debug)]
pub struct McpToolResult {
    pub content: String,
    pub images: Vec<String>,
    pub is_error: bool,
}

fn convert(result: CallToolResult) -> McpToolResult {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut is_error = result.is_error.unwrap_or(false);
    for block in result.content {
        match block {
            ContentBlock::Text(content) => text.push(content.text),
            ContentBlock::Image(content) => {
                images.push(content.data);
            }
            ContentBlock::Resource(content) => match content.resource {
                ResourceContents::TextResourceContents { text: content, .. } => text.push(content),
                ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } => match mime_type {
                    Some(mime)
                        if mime.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/")) =>
                    {
                        images.push(blob);
                    }
                    mime => {
                        text.push(format!(
                            "Unsupported embedded binary resource: {uri} ({})",
                            mime.as_deref().unwrap_or("unknown MIME type")
                        ));
                        is_error = true;
                    }
                },
                _ => {
                    text.push("MCP returned an unsupported resource.".into());
                    is_error = true;
                }
            },
            ContentBlock::ResourceLink(_) => {
                text.push(serde_json::to_string(&block).expect("resource link is JSON"));
            }
            ContentBlock::Audio(content) => {
                text.push(format!(
                    "MCP returned audio ({}); this model input supports text and images.",
                    content.mime_type
                ));
                is_error = true;
            }
            _ => {
                text.push("MCP returned an unsupported content block.".into());
                is_error = true;
            }
        }
    }
    if let Some(content) = result.structured_content
        && !text
            .iter()
            .any(|text| serde_json::from_str::<serde_json::Value>(text).ok().as_ref() == Some(&content))
    {
        text.push(serde_json::to_string(&content).expect("structured content is JSON"));
    }
    McpToolResult {
        content: text.join("\n\n"),
        images,
        is_error,
    }
}
