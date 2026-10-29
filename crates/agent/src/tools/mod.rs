//! 内建工具与 MCP 工具的注册、参数校验和执行分发。
//!
//! 工具在当前进程内运行，继承宿主权限。`ToolRegistrySnapshot` 汇总本任务开着的内建工具和
//! 本轮发现的 MCP 工具定义，生成发给模型的工具 Schema，并在收到工具调用时校验参数、分发执行。

mod bash;
mod edit;
mod edit_diff;
mod mcp;
mod output;
mod question;
mod read;
mod registry;
mod skill;
mod truncate;
mod view_image;

pub use bash::{BashTaskUpdate, BashTasks};
pub(crate) use registry::ToolContext;
pub use registry::ToolExecution;
pub(crate) use registry::{ABORTED_MESSAGE, error_result};
pub(crate) use registry::{
    PreparedTool, ToolRegistrySnapshot, bash_tool, edit_tool, question_tool, read_tool, task_tools,
    view_image_tool,
};
pub(crate) use skill::spec as skill_schema;
