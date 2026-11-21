//! 创建或整文件覆写 UTF-8 文本文件；与 `edit` 共用进程级路径锁、原子写盘和差异展示。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::edit::{file_lock_key, format_diff_result, read_opened_text_file, with_file_lock};
use super::registry::{ABORTED_MESSAGE, ToolExecution, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteArgs {
    path: String,
    content: String,
}

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: "write".into(),
        description: "Create a new UTF-8 text file or overwrite an existing text file with full content. Automatically creates missing parent directories and writes atomically. Prefer edit for targeted changes to existing files.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write, relative to the working directory or absolute" },
                "content": { "type": "string", "description": "Full UTF-8 text content to write to the file" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    }
}

pub(crate) async fn execute(args: WriteArgs, cwd: PathBuf, signal: CancellationToken) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if args.path.trim().is_empty() {
        return error_result("path must not be empty");
    }
    if args.content.contains('\0') {
        return error_result("write only supports UTF-8 text content without NUL bytes");
    }
    let target = cwd.join(&args.path);
    let key_target = target.clone();
    let key = match tokio::task::spawn_blocking(move || file_lock_key(&key_target))
        .await
        .expect("tool worker completes while the runtime is running")
    {
        Ok(key) => key,
        Err(error) => return error_result(format!("Could not write {}: {error}", args.path)),
    };
    with_file_lock(key, signal, move |signal| execute_locked(&args, &target, signal)).await
}

fn execute_locked(
    args: &WriteArgs,
    target: &Path,
    signal: &CancellationToken,
) -> Result<ToolExecution, String> {
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    let (before, existed) = match std::fs::File::open(target) {
        Ok(file) => (read_opened_text_file(file, &args.path)?, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(error) => return Err(format!("Could not inspect {}: {error}", args.path)),
    };
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create parent directory for {}: {error}", args.path))?;
    }
    // 覆写已存在文件且原文件以 CRLF 换行时，若传入纯 LF 则保留 CRLF 约定。
    let content = if existed && before.contains("\r\n") && !args.content.contains("\r\n") {
        args.content.replace('\n', "\r\n")
    } else {
        args.content.clone()
    };
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    app_core::write_bytes_atomic(target, content.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", args.path))?;
    let verb = if existed { "Wrote" } else { "Created" };
    Ok(format_diff_result(verb, &args.path, &before, &content))
}
