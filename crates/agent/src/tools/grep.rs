//! grep 工具：进程内递归按正则逐文件逐行匹配（跳过 .git/target/node_modules
//! 与二进制文件），输出 `path:line:text`，匹配上限 500 条，超出截断并提示。

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use regex::Regex;
use serde_json::{Value, json};

use super::glob::glob_regex;
use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::walk::{to_cwd_relative, walk_files};

pub(crate) const DESCRIPTION: &str = "Search file contents with a regular expression, recursively from path (default: the working directory). Outputs one line per match as path:line:text. Skips .git/target/node_modules and binary files. include is a glob filter on matched paths. Results are capped at 500 lines; if the cap is hit, narrow the pattern or include.";

const MAX_MATCHES: usize = 500;
/// 单行输出文本的最大长度；超长行只保留前缀。
const MAX_LINE_OUTPUT_BYTES: usize = 1024;
/// 文件头嗅探长度：出现 NUL 字节视为二进制并跳过。
const BINARY_SNIFF_BYTES: usize = 8192;

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Regular expression matched against each line" },
            "path": { "type": "string", "description": "Directory to search recursively (default: the working directory)" },
            "include": { "type": "string", "description": "Glob filter applied to matched file paths; only matching files are searched" },
        },
        "required": ["pattern"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "grep",
        description: DESCRIPTION,
        parameters: parameters(),
        execute,
    }
}

fn looks_binary(file: &mut File) -> bool {
    let mut buf = vec![0u8; BINARY_SNIFF_BYTES];
    let read = std::io::Read::read(file, &mut buf).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(0));
    buf[..read].contains(&0)
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(pattern) = ctx.args.get("pattern").and_then(Value::as_str) else {
        return error_result("missing required parameter \"pattern\"");
    };
    let path = ctx.args.get("path").and_then(Value::as_str).unwrap_or(".");
    let include = ctx.args.get("include").and_then(Value::as_str);
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let root = resolve_path(ctx.cwd, path);
    if !root.is_dir() {
        return error_result(format!("path is not a directory: {path}"));
    }
    let regex = match Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => {
            return error_result(format!("invalid regular expression {pattern:?}: {error}"));
        }
    };
    let include_regex = match include {
        Some(include) => match glob_regex(include) {
            Ok(regex) => Some(regex),
            Err(message) => return error_result(message),
        },
        None => None,
    };
    let mut output = String::new();
    let mut matches = 0usize;
    let mut scanned_files = 0usize;
    let _ = walk_files(&root, &mut |relative| {
        if matches >= MAX_MATCHES {
            return;
        }
        // include 过滤按 basename 匹配（路径长度无关）。
        let base_name = relative
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        if include_regex
            .as_ref()
            .is_some_and(|glob| !glob.is_match(base_name.as_ref()))
        {
            return;
        }
        scanned_files += 1;
        let Ok(mut file) = File::open(root.join(&relative)) else {
            return;
        };
        if looks_binary(&mut file) {
            return;
        }
        drop(file);
        let file = File::open(root.join(&relative)).expect("reopened file");
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut line_bytes = Vec::new();
        let mut line_number = 0u64;
        loop {
            if matches >= MAX_MATCHES {
                break;
            }
            let bytes = match reader.read_until(b'\n', &mut line_bytes) {
                Ok(0) => break,
                Ok(bytes) => bytes,
                Err(_) => break,
            };
            line_number += 1;
            if bytes > MAX_LINE_OUTPUT_BYTES {
                line_bytes.truncate(MAX_LINE_OUTPUT_BYTES);
                let mut text = String::from_utf8_lossy(&line_bytes).into_owned();
                text.push_str("...");
                if regex.is_match(&text) {
                    matches += 1;
                    output.push_str(&format!(
                        "{}:{line_number}:{text}\n",
                        to_cwd_relative(ctx.cwd, &root, &relative)
                    ));
                }
            } else {
                let mut text = String::from_utf8_lossy(&line_bytes).into_owned();
                if text.ends_with('\n') {
                    text.pop();
                }
                if text.ends_with('\r') {
                    text.pop();
                }
                if regex.is_match(&text) {
                    matches += 1;
                    output.push_str(&format!(
                        "{}:{line_number}:{text}\n",
                        to_cwd_relative(ctx.cwd, &root, &relative)
                    ));
                }
            }
            line_bytes.clear();
        }
    });
    if matches >= MAX_MATCHES {
        output.push_str(&format!(
            "\n[grep] results truncated at {MAX_MATCHES} matches; narrow the pattern or include filter."
        ));
    }
    if output.is_empty() {
        output = format!(
            "no matches for {pattern:?} under {path}{}",
            include
                .map(|include| format!(" (include: {include})"))
                .unwrap_or_default()
        );
    }
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    Ok(ToolExecution {
        content: output,
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
        fs::create_dir_all(dir.path().join("src")).expect("src dir");
        fs::write(
            dir.path().join("src/main.rs"),
            "// entry\nfn main() {\n    println!(\"todo\");\n}\n",
        )
        .expect("main");
        fs::write(
            dir.path().join("README.md"),
            "See src/main.rs for entry.\r\n",
        )
        .expect("readme");
        fs::write(dir.path().join("blob.bin"), b"\x00\x01\x02main()\n").expect("binary");
        fs::create_dir_all(dir.path().join("target")).expect("target");
        fs::write(dir.path().join("target/out.rs"), "fn main() {}\n").expect("target file");
        dir
    }

    #[test]
    fn grep_outputs_path_line_text_and_skips_binary_and_vcs() {
        let dir = layout();
        let result =
            execute(context(json!({ "pattern": r"main\(" }), dir.path())).expect("execute");
        assert!(!result.is_error);
        assert!(
            result.content.contains("src/main.rs:2:fn main() {"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("blob.bin"));
        assert!(!result.content.contains("target/out.rs"));
    }

    #[test]
    fn grep_tolerates_crlf_lines() {
        let dir = layout();
        let result =
            execute(context(json!({ "pattern": "src/main\\.rs" }), dir.path())).expect("execute");
        assert!(!result.is_error);
        assert!(
            result
                .content
                .contains("README.md:1:See src/main.rs for entry."),
            "{}",
            result.content
        );
    }

    #[test]
    fn grep_include_filter_limits_searched_files() {
        let dir = layout();
        let result = execute(context(
            json!({ "pattern": r"main\(", "include": "*.rs" }),
            dir.path(),
        ))
        .expect("execute");
        assert!(!result.is_error);
        assert!(result.content.contains("src/main.rs:2:"));
        assert!(!result.content.contains("README.md:1:"));
    }

    #[test]
    fn grep_truncates_at_match_limit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let content = (0..510)
            .map(|i| format!("line {i} marker"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path().join("many.txt"), content).expect("many lines");
        let result = execute(context(json!({ "pattern": "marker" }), dir.path())).expect("execute");
        assert!(!result.is_error);
        assert!(
            result.content.contains("results truncated at 500 matches"),
            "{}",
            result.content
        );
    }

    #[test]
    fn grep_rejects_invalid_regex() {
        let dir = tempfile::tempdir().expect("temp dir");
        let result = execute(context(json!({ "pattern": "(" }), dir.path())).expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("invalid regular expression"));
    }
}
