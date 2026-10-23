//! MCP 结果接入现有工具结果；图片复用校验、快照和会话保存路径。

use super::{ToolExecution, error_result};
use crate::image::InputImage;
use serde_json::{Map, Value};
use singularity_mcp::McpTool;
use tokio_util::sync::CancellationToken;

pub(super) async fn execute(
    tool: McpTool,
    args: Map<String, Value>,
    signal: &CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(super::ABORTED_MESSAGE);
    }
    let result = tool.call(args, signal).await;
    tokio::task::spawn_blocking(move || {
        let mut execution = match result {
            Ok(result) => {
                let mut execution = ToolExecution::text(result.content);
                execution.is_error = result.is_error;
                for data in result.images {
                    match InputImage::from_base64(format!("{}.image", tool.name), &data) {
                        Ok(image) => execution.images.push(image),
                        Err(error) => {
                            execution.content.push_str(&format!("\n\nMCP 图片无效：{error}"));
                            execution.is_error = true;
                        }
                    }
                }
                execution
            }
            Err(error) => error_result(error),
        };
        let mut output = super::output::CaptureState::default();
        output.ingest(&execution.content);
        let (content, note) = output.finish();
        execution.content = content;
        if let Some(note) = note {
            execution.content.push_str(&format!("\n\n{note}"));
        }
        execution
    })
    .await
    .expect("MCP result conversion completes while the runtime is running")
}
