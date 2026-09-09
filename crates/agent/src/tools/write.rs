//! write 工具：全量创建或覆盖写入指定文件（若父级目录不存在则自动递归创建）。

use std::fs;

use serde::Deserialize;
use serde_json::json;

use super::mutation::{lock_unpoisoned, mutation_lock, path_key};
use super::registry::{ExecuteContext, ToolExecution, error_result};

pub(crate) const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";
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
    let full_path = ctx.cwd.join(path);
    let key = path_key(ctx.cwd, path);
    let file_lock = mutation_lock(&key);
    let _guard = lock_unpoisoned(&file_lock);
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let before = match fs::read(&full_path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return error_result(format!(
                "Could not read file before writing: {path}. {error}"
            ));
        }
    };
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
    if let Err(error) =
        singularity_core::atomic_replace_workspace_file(&full_path, content.as_bytes())
    {
        return error_result(format!("Could not write file: {path}. {error}"));
    }
    ToolExecution {
        content: format!("Successfully wrote {} bytes to {path}", content.len()),
        diff: Some(super::edit::unified_diff(path, &before, content)),
        is_error: false,
        duration_ms: None,
    }
}
