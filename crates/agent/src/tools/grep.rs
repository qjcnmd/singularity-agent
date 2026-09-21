//! grep 工具：在进程内按正则逐文件、逐行递归匹配（跳过 .git/target/node_modules
//! 和二进制文件），输出 path:line:text，匹配条数和总字节数都有界。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use singularity_core::display_path;

use super::glob::glob_regex;
use super::line::MAX_READ_LINE_BYTES;
use super::registry::{ExecuteContext, ToolExecution, error_result};
use super::truncate::{DEFAULT_MAX_BYTES, default_max_kb};
use super::walk::{SearchWarnings, WalkControl, to_cwd_relative, walk_files};

const MAX_MATCHES: usize = 500;
/// 单行输出展示文本的最大字节数；命中行超长时只保留上限内、char 边界安全的前缀，再补 "..."。
const MAX_LINE_OUTPUT_BYTES: usize = 1024;
/// 文件头嗅探：出现 NUL 字节就当成二进制并跳过。读取器的缓冲容量就是嗅探窗口，
/// 一次 fill_buf 既拿到窗口又不消费数据，同一个读取器随后可以直接逐行搜索。
const BINARY_SNIFF_BYTES: usize = 8192;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Search file contents with a regular expression, recursively from path (default: the working directory). Outputs one line per match as path:line:text. Skips .git/target/node_modules and binary files. include is a glob filter on matched paths. Match output is capped at {MAX_MATCHES} matches or {}KB, whichever is reached first; individual line text is limited to {MAX_LINE_OUTPUT_BYTES} bytes plus an ellipsis. If a cap is hit, narrow the pattern or include.",
        default_max_kb()
    )
});

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
        description: &DESCRIPTION,
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

fn looks_binary(reader: &mut impl BufRead) -> std::io::Result<bool> {
    Ok(reader.fill_buf()?.contains(&0))
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
        // include 过滤：相对路径或文件名任一命中，这个文件就保留。
        if let Some(glob) = &include_regex {
            let rel_path = display_path(&relative);
            let base_name = relative
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            if !glob.is_match(&rel_path) && !glob.is_match(base_name.as_ref()) {
                return WalkControl::Continue;
            }
        }
        let full_path = root.join(&relative);
        let display = to_cwd_relative(ctx.cwd, &root, &relative);
        // 预算按调用那一刻的全局剩余量传进去；单文件扫描自己不做累计。
        let scan = match scan_file(
            &full_path,
            &display,
            &regex,
            MAX_MATCHES - matches,
            DEFAULT_MAX_BYTES.saturating_sub(output.len()),
            ctx.signal,
        ) {
            Ok(Some(scan)) => scan,
            Ok(None) => return WalkControl::Continue,
            Err(error) => {
                warnings.record(&full_path, &error);
                return WalkControl::Continue;
            }
        };
        if scan.over_limit_line {
            skipped_files += 1;
        }
        if let Some(error) = &scan.read_error {
            warnings.record(&full_path, error);
        }
        matches += scan.lines.len();
        for line in scan.lines {
            output.push_str(&line);
        }
        match scan.stop {
            None => WalkControl::Continue,
            // 只有输出预算耗尽才需要提示截断；取消不产生这种提示。
            Some(ScanStop::OutputBudget) => {
                byte_limit_hit = true;
                WalkControl::Stop
            }
            Some(ScanStop::Cancelled) => WalkControl::Stop,
        }
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
    ToolExecution::text(output)
}

pub(super) struct FileScan {
    /// 本文件按行号顺序产生的命中行，已经按 `path:line:text` 的展示格式拼好。
    pub(super) lines: Vec<String>,
    /// 本文件是否因为出现畸形的超长行而被整个跳过。
    pub(super) over_limit_line: bool,
    /// 扫描中途发生的读取错误。此前产生的命中行仍然有效，必须连警告一起保留。
    pub(super) read_error: Option<std::io::Error>,
    /// 必须停止整个遍历的原因；为 `None` 表示本文件已扫完，可以继续下一个候选。
    pub(super) stop: Option<ScanStop>,
}

/// 单文件扫描需要停止整个遍历的原因。两种原因的结果不同，调用方要分别处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScanStop {
    /// 下一条命中放不进剩余的输出字节预算，这一行不进入结果。
    OutputBudget,
    Cancelled,
}

/// 扫描一个候选文件：打开、判断是否二进制、逐行匹配，生成本文件自己的有界结果。
///
/// 这里只管单个文件：遍历顺序、include 过滤以及全局累计（命中总数、输出字节、警告、
/// 跳过计数）都留给调用方。`match_budget` 与 `byte_budget` 是调用那一刻的全局剩余
/// 预算，任何一个用完都通过 [`FileScan::stop`] 交回调用方停止遍历。文件打不开或嗅探
/// 失败返回 `Err`（此时还没有命中行）；二进制返回 `Ok(None)`，由调用方静默跳过。
pub(super) fn scan_file(
    path: &Path,
    display: &str,
    regex: &Regex,
    match_budget: usize,
    mut byte_budget: usize,
    signal: &singularity_core::CancellationToken,
) -> std::io::Result<Option<FileScan>> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(BINARY_SNIFF_BYTES, file);
    if looks_binary(&mut reader)? {
        return Ok(None);
    }
    let mut scan = FileScan {
        lines: Vec::new(),
        over_limit_line: false,
        read_error: None,
        stop: None,
    };
    let mut line_number = 0u64;
    while scan.lines.len() < match_budget {
        if signal.is_cancelled() {
            scan.stop = Some(ScanStop::Cancelled);
            break;
        }
        let bytes = match super::line::read_bounded_line(&mut reader, MAX_READ_LINE_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            // 畸形的超长行：跳过整个文件并计数，但不中止整次搜索。
            Err(super::line::LineFailure::OverLimit { .. }) => {
                scan.over_limit_line = true;
                break;
            }
            Err(super::line::LineFailure::Io(error)) => {
                scan.read_error = Some(error);
                break;
            }
        };
        line_number += 1;
        // 匹配的是剥掉行尾之后的整行：read_bounded_line 已去掉换行，但末行没有结尾
        // 换行时 CRLF 会残留一个 \r，在这里一并剥掉。展示截断只作用于输出文本。
        let mut line_end = bytes.len();
        if line_end > 0 && bytes[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let line = String::from_utf8_lossy(&bytes[..line_end]);
        if !regex.is_match(&line) {
            continue;
        }
        // 命中行超长时只截断展示文本（char 边界安全的前缀加 "..."），不影响匹配到的内容。
        let (prefix, truncated) = singularity_core::utf8_prefix(&line, MAX_LINE_OUTPUT_BYTES);
        let shown = if truncated {
            format!("{prefix}...")
        } else {
            prefix.to_string()
        };
        let entry = format!("{display}:{line_number}:{shown}\n");
        if entry.len() > byte_budget {
            scan.stop = Some(ScanStop::OutputBudget);
            break;
        }
        byte_budget -= entry.len();
        scan.lines.push(entry);
    }
    Ok(Some(scan))
}
