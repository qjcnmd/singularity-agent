//! grep 工具：进程内递归按正则逐文件逐行匹配（跳过 .git/target/node_modules
//! 与二进制文件），输出 `path:line:text`，匹配上限 500 条，超出截断并提示。

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};

use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::glob::glob_regex;
use super::line::MAX_READ_LINE_BYTES;
use super::registry::{ExecuteContext, ToolExecution, error_result};
use super::walk::{WalkControl, to_cwd_relative, walk_files};

pub(crate) const DESCRIPTION: &str = "Search file contents with a regular expression, recursively from path (default: the working directory). Outputs one line per match as path:line:text. Skips .git/target/node_modules and binary files. include is a glob filter on matched paths. Results are capped at 500 lines; if the cap is hit, narrow the pattern or include.";

const MAX_MATCHES: usize = 500;
/// 单行输出的展示文本最大字节数；超长命中行保留字节上限内、char 边界安全的前缀并追加 "..."。
const MAX_LINE_OUTPUT_BYTES: usize = 1024;
/// 文件头嗅探长度：出现 NUL 字节视为二进制并跳过。
const BINARY_SNIFF_BYTES: usize = 8192;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    pub(crate) include: Option<String>,
}

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
        replay: super::registry::ToolReplayClass::Safe,
    }
}

fn looks_binary(file: &mut File) -> bool {
    let mut buf = vec![0u8; BINARY_SNIFF_BYTES];
    let read = std::io::Read::read(file, &mut buf).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(0));
    buf[..read].contains(&0)
}

/// 命中行的展示文本：超过 [`MAX_LINE_OUTPUT_BYTES`] 的行截断为字节上限内、
/// char 边界安全的前缀并追加 "..."；截断只影响展示，不影响匹配结果集。
fn truncate_for_display(line: &str) -> String {
    if line.len() <= MAX_LINE_OUTPUT_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_OUTPUT_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

pub(crate) fn execute(args: &GrepArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = args.path.as_deref().unwrap_or(".");
    let include = args.include.as_deref();
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let root = ctx.cwd.join(path);
    if !root.is_dir() {
        return error_result(format!("path is not a directory: {path}"));
    }
    let regex = match Regex::new(&args.pattern) {
        Ok(regex) => regex,
        Err(error) => {
            return error_result(format!(
                "invalid regular expression {:?}: {error}",
                args.pattern
            ));
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
    let mut skipped_files = 0usize;
    if let Err(error) = walk_files(&root, &mut |relative| {
        if ctx.signal.is_cancelled() {
            return WalkControl::Stop;
        }
        if matches >= MAX_MATCHES {
            return WalkControl::Stop;
        }
        // include 过滤：相对路径与文件名任一命中即保留（docs §4 语义）。
        let rel_path = super::walk::display_path(&relative);
        let base_name = relative
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        if include_regex
            .as_ref()
            .is_some_and(|glob| !glob.is_match(&rel_path) && !glob.is_match(base_name.as_ref()))
        {
            return WalkControl::Continue;
        }
        scanned_files += 1;
        let Ok(mut file) = File::open(root.join(&relative)) else {
            return WalkControl::Continue;
        };
        if looks_binary(&mut file) {
            return WalkControl::Continue;
        }
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut line_number = 0u64;
        loop {
            if ctx.signal.is_cancelled() {
                return WalkControl::Stop;
            }
            if matches >= MAX_MATCHES {
                break;
            }
            let bytes = match super::line::read_bounded_line(&mut reader, MAX_READ_LINE_BYTES) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                // 畸形超长行：跳过整个文件并计数，不中止整个搜索。
                Err(super::line::LineFailure::OverLimit { .. }) => {
                    skipped_files += 1;
                    break;
                }
                Err(_) => break,
            };
            line_number += 1;
            // 正则对剥除行尾后的整行匹配；read_bounded_line 已剥除换行，
            // 无终态换行的 CRLF 末行残留的 \r 在此剥除，展示截断只作用于
            // 命中行的输出文本。
            let mut line_end = bytes.len();
            if line_end > 0 && bytes[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line = String::from_utf8_lossy(&bytes[..line_end]);
            if regex.is_match(&line) {
                matches += 1;
                output.push_str(&format!(
                    "{}:{line_number}:{}\n",
                    to_cwd_relative(ctx.cwd, &root, &relative),
                    truncate_for_display(&line),
                ));
            }
        }
        WalkControl::Continue
    }) {
        return error_result(format!("failed to walk {path}: {error}"));
    }
    if matches >= MAX_MATCHES {
        output.push_str(&format!(
            "\n[grep] results truncated at {MAX_MATCHES} matches; narrow the pattern or include filter."
        ));
    }
    if skipped_files > 0 {
        output.push_str(&format!(
            "\n[grep] {skipped_files} file(s) skipped: line exceeds {MAX_READ_LINE_BYTES} bytes"
        ));
    }
    if output.is_empty() {
        output = format!(
            "no matches for {:?} under {path}{}",
            args.pattern,
            include
                .map(|include| format!(" (include: {include})"))
                .unwrap_or_default()
        );
    }
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    ToolExecution {
        content: output,
        is_error: false,
    }
}
