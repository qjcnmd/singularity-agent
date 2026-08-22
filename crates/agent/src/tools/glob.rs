//! glob 工具：进程内递归按文件名模式匹配（跳过 .git/target/node_modules），
//! 结果上限 200 条，超出截断并提示。

use regex::Regex;
use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::walk::{display_path, to_cwd_relative, walk_files};

pub(crate) const DESCRIPTION: &str = "Find files and directories? No: find files whose path matches a glob pattern, searched recursively from path (default: the working directory). Pattern syntax: * matches any characters except /, ? matches exactly one character except /, ** matches any number of directories (including zero). Skips .git/target/node_modules. Results are capped at 200 entries; if the cap is hit, narrow the pattern.";

const MAX_MATCHES: usize = 200;

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern matched against paths relative to path" },
            "path": { "type": "string", "description": "Directory to search recursively (default: the working directory)" },
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "glob",
        description: DESCRIPTION,
        parameters: parameters(),
        execute,
    }
}

/// 把 glob 模式编译为正则：`*`/`?` 不跨 `/`，`**` 跨任意层目录。
pub(crate) fn glob_regex(pattern: &str) -> Result<Regex, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    // `**` 独占段时跨任意目录层（含零层）；段内退化普通星号。
                    if chars.get(i + 2) == Some(&'/') {
                        out.push_str("(?:.*/)?");
                        i += 3;
                        continue;
                    }
                    out.push_str("[^/]*");
                    i += 2;
                    continue;
                }
                out.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '\\' => {
                if let Some(next) = chars.get(i + 1) {
                    out.push_str(&regex::escape(&next.to_string()));
                    i += 2;
                    continue;
                }
                out.push_str(&regex::escape("\\"));
                i += 1;
            }
            character => {
                out.push_str(&regex::escape(&character.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    Regex::new(&out).map_err(|error| format!("invalid glob pattern {pattern:?}: {error}"))
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(pattern) = ctx.args.get("pattern").and_then(Value::as_str) else {
        return error_result("missing required parameter \"pattern\"");
    };
    let path = ctx.args.get("path").and_then(Value::as_str).unwrap_or(".");
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let root = resolve_path(ctx.cwd, path);
    if !root.is_dir() {
        return error_result(format!("path is not a directory: {path}"));
    }
    let regex = match glob_regex(pattern) {
        Ok(regex) => regex,
        Err(message) => return error_result(message),
    };
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    let _ = walk_files(&root, &mut |relative| {
        scanned += 1;
        if matches.len() < MAX_MATCHES && regex.is_match(&display_path(&relative)) {
            matches.push(to_cwd_relative(ctx.cwd, &root, &relative));
        }
    });
    matches.sort();
    let mut content = matches.join("\n");
    let truncated = scanned > MAX_MATCHES;
    if truncated {
        content.push_str("\n[glob] results truncated: showing first ");
        content.push_str(&MAX_MATCHES.to_string());
        content.push_str(" of ");
        content.push_str(&scanned.to_string());
        content.push_str(" scanned files; narrow the pattern to see the rest.");
    }
    if content.is_empty() {
        content = format!("no files matched {pattern:?} under {path}");
    }
    Ok(ToolExecution {
        content,
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::context;
    use std::fs;

    fn layout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("src/deep")).expect("src dirs");
        fs::write(dir.path().join("README.md"), "hi").expect("readme");
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").expect("main");
        fs::write(dir.path().join("src/lib.rs"), "").expect("lib");
        fs::write(dir.path().join("src/deep/util.rs"), "").expect("util");
        fs::create_dir_all(dir.path().join("target/build")).expect("target");
        fs::write(dir.path().join("target/build/out.rs"), "").expect("target file");
        fs::create_dir_all(dir.path().join(".git/hooks")).expect("git");
        fs::write(dir.path().join(".git/hooks/pre-commit"), "").expect("hook");
        dir
    }

    #[test]
    fn glob_matches_files_recursively_and_skips_vcs_dirs() {
        let dir = layout();
        let result =
            execute(context(json!({ "pattern": "**/*.rs" }), dir.path())).expect("execute");
        assert!(!result.is_error);
        assert!(result.content.contains("src/main.rs"));
        assert!(result.content.contains("src/deep/util.rs"));
        assert!(!result.content.contains("target/build/out.rs"));
        assert!(!result.content.contains(".git/hooks/pre-commit"));
    }

    #[test]
    fn glob_single_star_does_not_cross_directories() {
        let dir = layout();
        let result =
            execute(context(json!({ "pattern": "src/*.rs" }), dir.path())).expect("execute");
        assert!(result.content.contains("src/main.rs"));
        assert!(!result.content.contains("src/deep/util.rs"));
    }

    #[test]
    fn glob_prefix_requires_slash() {
        let dir = layout();
        let no_prefix =
            execute(context(json!({ "pattern": "**origin" }), dir.path())).expect("execute");
        assert_eq!(no_prefix.content, "no files matched \"**origin\" under .");
        let prefixed =
            execute(context(json!({ "pattern": "**/*.md" }), dir.path())).expect("execute");
        assert!(prefixed.content.contains("README.md"));
    }

    #[test]
    fn glob_rooted_at_the_given_path() {
        let dir = layout();
        let result = execute(context(
            json!({ "pattern": "*.rs", "path": "src" }),
            dir.path(),
        ))
        .expect("execute");
        assert!(result.content.contains("src/main.rs"));
        assert!(!result.content.contains("src/deep/util.rs"));
        assert!(!result.content.contains("README.md"));
    }

    #[test]
    fn glob_rejects_missing_required_parameter() {
        let dir = layout();
        let result = execute(context(json!({}), dir.path())).expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("missing required parameter \"pattern\"")
        );
    }
}
