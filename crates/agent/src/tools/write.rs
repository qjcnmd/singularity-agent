//! write 工具：写文件（创建/覆盖/自动建父目录），对齐 Pi write 语义。

use std::fs;

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};

pub(crate) const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" },
        },
        "required": ["path", "content"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "write",
        description: DESCRIPTION,
        parameters: parameters(),
        execute: execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(path) = ctx.args.get("path").and_then(Value::as_str) else {
        return error_result("missing required parameter \"path\"");
    };
    let Some(content) = ctx.args.get("content").and_then(Value::as_str) else {
        return error_result("missing required parameter \"content\"");
    };
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let full_path = resolve_path(ctx.cwd, path);
    if let Some(parent) = full_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = fs::create_dir_all(parent) {
                return error_result(format!(
                    "Could not write file: {path}. Failed to create parent directories: {error}"
                ));
            }
        }
    }
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    if let Err(error) = fs::write(&full_path, content) {
        return error_result(format!("Could not write file: {path}. {error}"));
    }
    Ok(ToolExecution {
        content: format!("Successfully wrote {} bytes to {path}", content.len()),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
    use tempfile::tempdir;

    fn execute_write(cwd: &std::path::Path, path: &str, content: &str) -> ToolExecution {
        ToolRegistry::new()
            .execute(
                "write",
                context(json!({ "path": path, "content": content }), cwd),
            )
            .expect("execute")
    }

    #[test]
    fn creates_file_with_content() {
        let dir = tempdir().expect("temp dir");
        let result = execute_write(dir.path(), "out.txt", "hello");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result
                .content
                .contains("Successfully wrote 5 bytes to out.txt")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).expect("read back"),
            "hello"
        );
    }

    #[test]
    fn creates_parent_directories() {
        let dir = tempdir().expect("temp dir");
        let result = execute_write(dir.path(), "a/b/c.txt", "nested");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).expect("read back"),
            "nested"
        );
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = tempdir().expect("temp dir");
        let first = execute_write(dir.path(), "out.txt", "first");
        assert!(!first.is_error);
        let second = execute_write(dir.path(), "out.txt", "second");
        assert!(!second.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).expect("read back"),
            "second"
        );
    }
}
