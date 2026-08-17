//! 模型可见工具的单一注册事实源（Pi 式默认工具集：read/bash/edit/write）。
//!
//! 工具在进程内执行并继承宿主权限：无沙箱、无权限投影、无审批链（Phase 3 范围）。
//! Phase 2d 的 agent loop 从这里读取工具列表，把 `ToolSpec` 的 name/description/parameters
//! 注册为模型可见 schema，并调用 `ToolRegistry::execute` 执行模型产生的工具调用。

pub mod bash;
pub mod edit;
pub mod read;
pub mod registry;
pub mod write;

mod truncate;

pub use registry::{
    ExecuteContext, PreparedTool, ToolError, ToolExecution, ToolExecutionMode, ToolPreflight,
    ToolRegistry, ToolSpec,
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
            mutation_queue: None,
        }
    }
}
