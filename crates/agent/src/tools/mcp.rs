//! MCP 结果接入现有工具结果；图片复用校验、快照和会话保存路径。

use super::{ToolExecution, error_result};
use crate::image::InputImage;
use serde_json::{Map, Value};
use singularity_mcp::McpTool;
use singularity_protocol::ImageUpload;
use tokio_util::sync::CancellationToken;

pub(super) async fn execute(
    tool: McpTool,
    args: Map<String, Value>,
    signal: &CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(super::ABORTED_MESSAGE);
    }
    let result = match tool.call(args, signal).await {
        Ok(result) => result,
        Err(error) => return error_result(error),
    };
    tokio::task::spawn_blocking(move || {
        let mut execution = ToolExecution::text(result.content);
        execution.is_error = result.is_error;
        for data_url in result.images {
            match InputImage::upload(ImageUpload {
                name: format!("{}.image", tool.name),
                data_url,
            }) {
                Ok(image) => execution.images.push(image),
                Err(error) => {
                    execution
                        .content
                        .push_str(&format!("\n\nMCP 图片无效：{error}"));
                    execution.is_error = true;
                }
            }
        }
        execution
    })
    .await
    .expect("MCP result conversion completes while the runtime is running")
}
