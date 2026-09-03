//! write 工具：全量创建或覆盖写入指定文件（若父级目录不存在则自动递归创建）。
//!
//! - **防误覆盖**：目标已经存在时，必须是本会话 `read` 过（或刚 `write`/`edit` 过）
//!   且版本未变的文件才允许整份盖写；新建文件不受此限。看到"不存在"之后目标被
//!   别人创建出来，本次写入同样拒绝，而不是撞掉对方。事实源见 `observe`。

use std::fs;

use serde::Deserialize;
use serde_json::json;

use super::batch::path_key;
use super::observe::{Observed, current_version};
use super::registry::{ExecuteContext, ToolExecution, error_result};

pub(crate) const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does; overwriting an existing file requires that it was read earlier in this session. Automatically creates parent directories.";
pub(crate) const NAME: &str = "write";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteArgs {
    pub(crate) path: String,
    pub(crate) content: String,
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: NAME,
        snippet: "Create or overwrite files",
        description: DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" },
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(args: &WriteArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = &args.path;
    let content = &args.content;
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let full_path = ctx.cwd.join(path);
    // 防误覆盖闸门：分"见过这一版"与"没见过"两条判据，两条都只在目标确实
    // 存在时才拦——目标不存在就是新建，无需任何前置观察。
    let key = path_key(ctx.cwd, path);
    let existing = current_version(&full_path);
    match ctx.observed.observed(&key) {
        Observed::Present(version) => {
            if existing != Some(version) {
                return error_result(format!(
                    "Could not write file: {path}. It changed since it was read; read it again, then retry."
                ));
            }
        }
        Observed::Unseen | Observed::Absent => {
            if existing.is_some() {
                return error_result(format!(
                    "Could not write file: {path}. It already exists but has not been read in this session; read it first, then retry."
                ));
            }
        }
    }
    if let Some(parent) = full_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return error_result(format!(
            "Could not write file: {path}. Failed to create parent directories: {error}"
        ));
    }
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    if let Err(error) = singularity_core::atomic_replace_bytes(&full_path, content.as_bytes()) {
        return error_result(format!("Could not write file: {path}. {error}"));
    }
    // 写出的内容本会话已经知道：补记新版本，之后可直接 edit 或再次 write。
    if let Some(version) = current_version(&full_path) {
        ctx.observed.record(&key, Observed::Present(version));
    }
    ToolExecution {
        content: format!("Successfully wrote {} bytes to {path}", content.len()),
        is_error: false,
    }
}
