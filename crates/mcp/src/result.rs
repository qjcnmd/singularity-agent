//! 在 MCP 边界转换 SDK 内容，宿主只接收文字和待校验的图片。

use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// 工具结果；图片是服务器返回的 data URL，由宿主校验和保存。
pub struct McpToolResult {
    pub content: String,
    pub images: Vec<String>,
    pub is_error: bool,
}

pub(crate) fn convert(result: CallToolResult) -> McpToolResult {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut is_error = result.is_error.unwrap_or(false);
    for block in result.content {
        match block {
            ContentBlock::Text(content) => text.push(content.text),
            ContentBlock::Image(content) => {
                images.push(format!("data:{};base64,{}", content.mime_type, content.data));
            }
            ContentBlock::Resource(content) => match content.resource {
                ResourceContents::TextResourceContents { text: content, .. } => text.push(content),
                ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } => match mime_type {
                    Some(mime) if mime.starts_with("image/") => {
                        images.push(format!("data:{mime};base64,{blob}"));
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
    if let Some(content) = result.structured_content {
        let structured = serde_json::to_string(&content).expect("structured content is JSON");
        if !text.contains(&structured) {
            text.push(structured);
        }
    }
    McpToolResult {
        content: text.join("\n\n"),
        images,
        is_error,
    }
}
