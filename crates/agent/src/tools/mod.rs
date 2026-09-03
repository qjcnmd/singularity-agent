//! Singularity 内建工具注册与执行模块。
//!
//! 提供面向代码研发的核心工具集：`read`、`glob`、`grep`、`bash`、`edit` 与 `write`。
//! 工具在进程内执行并继承当前运行权限（不设路径限制，也不做审批）。AgentLoop
//! 从此处读取工具定义并生成模型协议的 Tool Schemas，并在收到模型 ToolCall 时
//! 通过 `ToolRegistrySnapshot` 完成参数校验与安全分发。`write`/`edit` 另受
//! `observe` 的防误覆盖约束：没见过的文件不能整份盖，见过的版本变了不能改。

pub mod bash;
pub mod batch;
pub mod edit;
pub mod glob;
pub mod grep;
pub(crate) mod line;
pub mod observe;
pub mod read;
pub mod registry;
pub mod write;

mod truncate;
mod walk;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub(crate) use registry::{ExecuteContext, PreparedTool, ToolPreflight, error_result};
pub use registry::{ToolExecution, ToolRegistrySnapshot};
