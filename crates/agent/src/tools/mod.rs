//! Singularity 内建工具的注册与执行模块。
//!
//! 工具在进程内执行，继承当前运行的权限（不限制路径，也不做审批）。Agent 从这里
//! 读取工具定义、生成模型协议的 Tool Schemas，并在收到模型 ToolCall 时经
//! ToolRegistrySnapshot 完成参数校验与执行分发。

pub mod bash;
mod edit;
mod glob;
mod grep;
pub(crate) mod line;
mod mcp;
pub(crate) mod mutation;
mod question;
mod read;
mod registry;
mod write;

mod truncate;
mod walk;

pub use registry::ToolExecution;
pub(crate) use registry::{ABORTED_MESSAGE, error_result};
pub(crate) use registry::{PreparedTool, ToolRegistrySnapshot};
