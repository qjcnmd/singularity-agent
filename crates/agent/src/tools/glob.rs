//! glob 工具：在进程内按文件名模式递归匹配（跳过 .git/target/node_modules），
//! 结果最多 200 条；只有确实存在第 201 个匹配时才会停下并提示截断。

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use singularity_core::display_path;

use super::registry::{ExecuteContext, ToolExecution, error_result};
use super::walk::{WalkControl, to_cwd_relative, walk_files};

const MAX_MATCHES: usize = 200;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Find files whose path matches a glob pattern, searched recursively from path (default: the working directory). Pattern syntax: * matches any characters except /, ? matches exactly one character except /, ** matches any number of directories (including zero). Skips .git/target/node_modules. Results are capped at {MAX_MATCHES} entries; if the cap is hit, narrow the pattern."
    )
});

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "glob",
        snippet: "Find files by glob pattern",
        description: &DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern matched against paths relative to path" },
                "path": { "type": "string", "description": "Directory to search recursively (default: the working directory)" },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
    }
}

/// 把 glob 模式编译成正则：`*` 和 `?` 不跨 `/`，`**` 可以跨任意层目录。
pub(crate) fn glob_regex(pattern: &str) -> Result<Regex, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    // ** 独占一段时跨任意目录层（含零层）；末尾的 **（后面没有
                    // /）同样跨层，例如 src/** 能匹配深层文件；夹在段中则退化成普通星号。
                    if chars.get(i + 2) == Some(&'/') {
                        out.push_str("(?:.*/)?");
                        i += 3;
                        continue;
                    }
                    if chars.get(i + 2).is_none() {
                        out.push_str("(?:.*)?");
                        i += 2;
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

pub(crate) fn execute(args: &GlobArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = args.path.as_deref().unwrap_or(".");
    let root = match super::walk::search_root(ctx.cwd, path) {
        Ok(root) => root,
        Err(message) => return error_result(message),
    };
    let regex = match glob_regex(&args.pattern) {
        Ok(regex) => regex,
        Err(message) => return error_result(message),
    };
    let mut matches = Vec::new();
    let mut truncated = false;
    let warnings = match walk_files(&root, ctx.signal, &mut |relative| {
        // 只有「确实又发现了一个超限匹配」才能证明被截断：仅仅达到上限不算证据，
        // 否则恰好取满上限、后面文件都不匹配时也会误报还有剩余结果。
        if regex.is_match(&display_path(&relative)) {
            if matches.len() >= MAX_MATCHES {
                truncated = true;
                return WalkControl::Stop;
            }
            matches.push(to_cwd_relative(ctx.cwd, &root, &relative));
        }
        WalkControl::Continue
    }) {
        Ok(warnings) => warnings,
        Err(error) => return error_result(format!("failed to walk {path}: {error}")),
    };
    matches.sort();
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let mut content = matches.join("\n");
    if truncated {
        content.push_str("\n[glob] results truncated: showing first ");
        content.push_str(&MAX_MATCHES.to_string());
        content.push_str(" matching files; narrow the pattern to see the rest.");
    }
    if content.is_empty() {
        content = format!("no files matched {:?} under {path}", args.pattern);
    }
    warnings.append_to(&mut content);
    ToolExecution::text(content)
}
