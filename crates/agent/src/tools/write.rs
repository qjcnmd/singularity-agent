//! write 工具：全量创建或覆盖写入指定文件（若父级目录不存在则自动递归创建）。

use std::fs;

use serde::Deserialize;
use serde_json::{Value, json};

use super::registry::{
    ExecuteContext, ToolError, ToolExecution, deserialize_args_or_error, error_result,
    resolve_path, validate_args,
};

pub(crate) const DESCRIPTION: &str = "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

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
        validate: validate_args::<WriteArgs>,
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let args = match deserialize_args_or_error::<WriteArgs>(&ctx.args) {
        Ok(args) => args,
        Err(execution) => return Ok(execution),
    };
    let path = &args.path;
    let content = &args.content;
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let full_path = resolve_path(ctx.cwd, path);
    if let Some(parent) = full_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return error_result(format!(
            "Could not write file: {path}. Failed to create parent directories: {error}"
        ));
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
    fn writes_nested_file_and_overwrites_existing_content() {
        let dir = tempdir().expect("temp dir");
        let result = execute_write(dir.path(), "a/b/c.txt", "nested");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).expect("read back"),
            "nested"
        );

        let second = execute_write(dir.path(), "a/b/c.txt", "second");
        assert!(!second.is_error, "content: {}", second.content);
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).expect("read back"),
            "second"
        );
    }
}
