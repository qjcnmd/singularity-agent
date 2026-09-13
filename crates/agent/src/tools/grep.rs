//! grep 工具：进程内递归按正则逐文件逐行匹配（跳过 .git/target/node_modules
//! 与二进制文件），输出 path:line:text，匹配条数与总字节数有界。

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};

use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use singularity_core::display_path;

use super::glob::glob_regex;
use super::line::MAX_READ_LINE_BYTES;
use super::registry::{ExecuteContext, ToolExecution, error_result};
use super::truncate::DEFAULT_MAX_BYTES;
use super::walk::{SearchWarnings, WalkControl, to_cwd_relative, walk_files};

pub(crate) const DESCRIPTION: &str = "Search file contents with a regular expression, recursively from path (default: the working directory). Outputs one line per match as path:line:text. Skips .git/target/node_modules and binary files. include is a glob filter on matched paths. Match output is capped at 500 lines or 50KB, whichever is reached first; individual line text is limited to 1024 bytes plus an ellipsis. If a cap is hit, narrow the pattern or include.";

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

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "grep",
        snippet: "Search file contents for patterns",
        description: DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression matched against each line" },
                "path": { "type": "string", "description": "Directory to search recursively (default: the working directory)" },
                "include": { "type": "string", "description": "Glob filter applied to matched file paths; only matching files are searched" },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        }),
    }
}

fn looks_binary(file: &mut File) -> std::io::Result<bool> {
    let mut buf = vec![0u8; BINARY_SNIFF_BYTES];
    let read = std::io::Read::read(file, &mut buf)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(buf[..read].contains(&0))
}

pub(crate) fn execute(args: &GrepArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = args.path.as_deref().unwrap_or(".");
    let include = args.include.as_deref();
    let root = match super::walk::search_root(ctx.cwd, path) {
        Ok(root) => root,
        Err(message) => return error_result(message),
    };
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
    let mut byte_limit_hit = false;
    let mut skipped_files = 0usize;
    let mut warnings = SearchWarnings::default();
    let walk_warnings = walk_files(&root, ctx.signal, &mut |relative| {
        if ctx.signal.is_cancelled() {
            return WalkControl::Stop;
        }
        if matches >= MAX_MATCHES {
            return WalkControl::Stop;
        }
        // include 过滤：相对路径与文件名任一命中即保留（docs §4 语义）。
        let rel_path = display_path(&relative);
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
        let full_path = root.join(&relative);
        let mut file = match File::open(&full_path) {
            Ok(file) => file,
            Err(error) => {
                warnings.record(&full_path, &error);
                return WalkControl::Continue;
            }
        };
        match looks_binary(&mut file) {
            Ok(true) => return WalkControl::Continue,
            Ok(false) => {}
            Err(error) => {
                warnings.record(&full_path, &error);
                return WalkControl::Continue;
            }
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
                Err(super::line::LineFailure::Io(error)) => {
                    warnings.record(&full_path, &error);
                    break;
                }
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
                // 超长命中行只截断展示（char 边界安全前缀 + "..."），不影响匹配集。
                let (prefix, truncated) =
                    singularity_core::utf8_prefix(&line, MAX_LINE_OUTPUT_BYTES);
                let shown = if truncated {
                    format!("{prefix}...")
                } else {
                    prefix.to_string()
                };
                let entry = format!(
                    "{}:{line_number}:{shown}\n",
                    to_cwd_relative(ctx.cwd, &root, &relative),
                );
                if output.len() + entry.len() > DEFAULT_MAX_BYTES {
                    byte_limit_hit = true;
                    return WalkControl::Stop;
                }
                output.push_str(&entry);
                matches += 1;
            }
        }
        WalkControl::Continue
    });
    match walk_warnings {
        Ok(walk_warnings) => warnings.merge(walk_warnings),
        Err(error) => return error_result(format!("failed to walk {path}: {error}")),
    }
    if byte_limit_hit {
        output.push_str(&format!(
            "\n[grep] results truncated at {matches} matches by the {DEFAULT_MAX_BYTES}-byte output limit; narrow the pattern or include filter."
        ));
    } else if matches >= MAX_MATCHES {
        output.push_str(&format!(
            "\n[grep] search stopped at {MAX_MATCHES} matches; results may be incomplete. Narrow the pattern or include filter."
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
    warnings.append_to(&mut output);
    ToolExecution {
        content: output,
        is_error: false,
        diff: None,
        duration_ms: None,
    }
}
