//! Singularity 内建工具注册与执行模块。
//!
//! 提供面向代码研发的核心工具集：`read`、`glob`、`grep`、`bash`、`edit` 与 `write`。
//! 工具在进程内执行并继承当前运行权限。AgentLoop 从此处读取工具定义并生成
//! 模型协议的 Tool Schemas，并在收到模型 ToolCall 时通过 `ToolRegistry` 完成参数校验与安全分发。

pub mod bash;
pub mod batch;
pub mod edit;
pub mod glob;
pub mod grep;
mod line;
mod path;
pub mod read;
pub mod registry;
pub mod write;

mod truncate;
mod walk;

pub use registry::{
    ExecuteContext, PreparedTool, ToolError, ToolExecution, ToolPreflight, ToolRegistry, ToolSpec,
};

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::Path;

    /// 测试用 `ExecuteContext`：无取消信号、无流式回调。
    pub(crate) fn context<'a>(args: serde_json::Value, cwd: &'a Path) -> ExecuteContext<'a> {
        ExecuteContext {
            args,
            cwd,
            signal: None,
            on_update: None,
        }
    }
}
