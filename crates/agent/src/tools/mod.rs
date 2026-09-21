//! Singularity 内建工具的注册与执行模块。
//!
//! 工具在进程内执行，继承当前运行的权限（不限制路径，也不做审批）。AgentLoop 从这里
//! 读取工具定义、生成模型协议的 Tool Schemas，并在收到模型 ToolCall 时经
//! ToolRegistrySnapshot 完成参数校验与执行分发。

pub mod bash;
pub(crate) mod batch;
mod edit;
mod glob;
mod grep;
pub(crate) mod line;
pub(crate) mod mutation;
mod read;
mod registry;
mod write;

mod truncate;
mod walk;

pub(crate) use registry::{ExecuteContext, PreparedTool, error_result};
pub use registry::{ToolExecution, ToolRegistrySnapshot};
