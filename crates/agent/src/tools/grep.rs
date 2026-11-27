use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::path::Path;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::glob::{SearchTarget, build_walker, compile_glob, format_display_path, inspect_search_target};
use super::registry::{ABORTED_MESSAGE, DEFAULT_MAX_BYTES, ToolExecution, default_max_kb, error_result};

const DEFAULT_GREP_LIMIT: usize = 100;
const MAX_GREP_LIMIT: usize = 500;
const MAX_GREP_CONTEXT: usize = 10;
const MAX_LINE_CHARS: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    pub(crate) glob: Option<String>,
    #[serde(default)]
    pub(crate) ignore_case: bool,
    #[serde(default)]
    pub(crate) literal: bool,
    pub(crate) context: Option<u64>,
    pub(crate) limit: Option<u64>,
}

impl GrepArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.pattern.is_empty() {
            return Err(error_result("invalid tool arguments: pattern must not be empty"));
        }
        if self.limit == Some(0) {
            return Err(error_result("invalid tool arguments: limit must be at least 1"));
        }
        Ok(self)
    }
}

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: "grep".to_string(),
        description: format!(
            "Search file contents using regex or literal matching, respecting .gitignore and skipping binary files and .git. Returns 'path:line: content' (context lines use 'path-line- content' separated by '--'). Capped at {DEFAULT_GREP_LIMIT} matches by default (max {MAX_GREP_LIMIT}), {MAX_LINE_CHARS} chars per line, and {}KB total.",
            default_max_kb()
        ),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern (or literal text when literal=true) to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (relative to workspace or absolute; defaults to workspace root)"
                },
                "glob": {
                    "type": "string",
                    "description": "Optional glob to filter searched files (e.g. '*.rs', 'src/**/*.ts')"
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "Case-insensitive matching (default false)"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as a literal string rather than regex (default false)"
                },
                "context": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_GREP_CONTEXT,
                    "description": format!("Number of context lines before and after each match (default 0, max {MAX_GREP_CONTEXT})")
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_GREP_LIMIT,
                    "description": format!("Maximum number of matching lines to return (default {DEFAULT_GREP_LIMIT}, max {MAX_GREP_LIMIT})")
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn execute(args: &GrepArgs, cwd: &Path, signal: &CancellationToken) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case)
        .fixed_strings(args.literal)
        .multi_line(true)
        .crlf(true)
        .line_terminator(None)
        .build(&args.pattern)
    {
        Ok(matcher) => matcher,
        Err(error) => return error_result(format!("Invalid regex pattern: {error}")),
    };
    let file_glob = match args.glob.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        Some(raw) => match compile_glob(raw) {
            Ok(compiled) => Some(compiled),
            Err(error) => return error,
        },
        None => None,
    };
    let context_lines = args
        .context
        .map_or(0, |value| usize::try_from(value).unwrap_or(MAX_GREP_CONTEXT).min(MAX_GREP_CONTEXT));
    let match_limit = args.limit.map_or(DEFAULT_GREP_LIMIT, |value| {
        usize::try_from(value).unwrap_or(MAX_GREP_LIMIT).clamp(1, MAX_GREP_LIMIT)
    });

    let (search_path, metadata) = match inspect_search_target(args.path.as_deref(), cwd) {
        Ok(SearchTarget::Skip) => return ToolExecution::text("No matches found."),
        Ok(SearchTarget::Ready(path, metadata)) => (path, metadata),
        Err(error) => return error,
    };

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(context_lines)
        .after_context(context_lines)
        .build();

    let mut state = SearchState {
        output: String::new(),
        match_count: 0,
        has_output: false,
        hit_match_limit: false,
        hit_byte_limit: false,
        cancelled: false,
    };
    let mut read_errors = 0usize;
    let mut first_read_error: Option<(String, String)> = None;

    if metadata.is_file() {
        let display = format_display_path(&search_path, cwd);
        let mut sink = state.sink(&display, signal, context_lines, match_limit);
        if let Err(error) = search_file(&mut searcher, &matcher, &search_path, signal, &mut sink) {
            if signal.is_cancelled() || state.cancelled {
                return error_result(ABORTED_MESSAGE);
            }
            let shown = args.path.as_deref().unwrap_or(&display);
            return error_result(format!("Could not search file: {shown}. {error}"));
        }
    } else if metadata.is_dir() {
        let include_hidden = file_glob.as_ref().is_some_and(|g| g.include_hidden);
        for entry in build_walker(&search_path, include_hidden, true).build() {
            if signal.is_cancelled() {
                return error_result(ABORTED_MESSAGE);
            }
            if state.should_stop() {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if let Some(compiled) = &file_glob {
                let Ok(rel) = entry.path().strip_prefix(&search_path) else {
                    continue;
                };
                let rel_slash = rel.to_string_lossy().replace('\\', "/");
                if !compiled.matcher.is_match(&rel_slash) {
                    continue;
                }
            }
            let display = format_display_path(entry.path(), cwd);
            let mut sink = state.sink(&display, signal, context_lines, match_limit);
            if let Err(error) = search_file(&mut searcher, &matcher, entry.path(), signal, &mut sink) {
                if signal.is_cancelled() || state.cancelled {
                    return error_result(ABORTED_MESSAGE);
                }
                read_errors += 1;
                if first_read_error.is_none() {
                    first_read_error = Some((display, error.to_string()));
                }
            }
        }
    } else {
        let shown = args.path.as_deref().unwrap_or(".");
        return error_result(format!("Path is neither a file nor a directory: {shown}"));
    }

    if state.cancelled || signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if state.match_count == 0 {
        if let Some((path, err)) = first_read_error {
            return error_result(format!("Could not search file: {path}. {err}"));
        }
        return ToolExecution::text("No matches found.");
    }

    if state.hit_byte_limit {
        let _ = write!(
            state.output,
            "\n\n[Output truncated at {}KB after {} matches. Narrow the path or glob filter.]",
            default_max_kb(),
            state.match_count
        );
    } else if state.hit_match_limit {
        let _ = write!(
            state.output,
            "\n\n[Stopped at match limit ({match_limit}). Narrow the path or glob filter, or increase limit.]"
        );
    }
    if let Some((path, err)) = first_read_error {
        let _ = write!(
            state.output,
            "\n\n[Note: skipped {read_errors} unreadable file(s); first: {path}: {err}]"
        );
    }

    ToolExecution::text(state.output)
}

struct CancellableReader<'a, R> {
    inner: R,
    signal: &'a CancellationToken,
}

impl<R: io::Read> io::Read for CancellableReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.signal.is_cancelled() {
            return Err(io::Error::other(ABORTED_MESSAGE));
        }
        self.inner.read(buf)
    }
}

fn search_file(
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    signal: &CancellationToken,
    sink: &mut MatchSink<'_>,
) -> io::Result<()> {
    if signal.is_cancelled() {
        return Err(io::Error::other(ABORTED_MESSAGE));
    }
    let file = std::fs::File::open(path)?;
    searcher.search_reader(matcher, CancellableReader { inner: file, signal }, sink)
}

struct SearchState {
    output: String,
    match_count: usize,
    has_output: bool,
    hit_match_limit: bool,
    hit_byte_limit: bool,
    cancelled: bool,
}

impl SearchState {
    fn should_stop(&self) -> bool {
        self.cancelled || self.hit_byte_limit || self.hit_match_limit
    }

    fn sink<'a>(
        &'a mut self,
        display_path: &'a str,
        signal: &'a CancellationToken,
        context_lines: usize,
        match_limit: usize,
    ) -> MatchSink<'a> {
        MatchSink {
            display_path,
            signal,
            context_lines,
            match_limit,
            state: self,
            file_emitted: false,
            remaining_after_context: 0,
        }
    }
}

struct MatchSink<'a> {
    display_path: &'a str,
    signal: &'a CancellationToken,
    context_lines: usize,
    match_limit: usize,
    state: &'a mut SearchState,
    file_emitted: bool,
    remaining_after_context: usize,
}

impl MatchSink<'_> {
    fn push_raw_line(&mut self, line: &str) -> bool {
        let sep = usize::from(!self.state.output.is_empty());
        if self.state.output.len() + sep + line.len() > DEFAULT_MAX_BYTES {
            self.state.hit_byte_limit = true;
            return false;
        }
        if !self.state.output.is_empty() {
            self.state.output.push('\n');
        }
        self.state.output.push_str(line);
        self.state.has_output = true;
        true
    }

    fn push_entry_line(&mut self, line: &str) -> bool {
        if self.signal.is_cancelled() {
            self.state.cancelled = true;
            return false;
        }
        if !self.file_emitted && self.state.has_output && self.context_lines > 0 && !self.push_raw_line("--")
        {
            return false;
        }
        if !self.push_raw_line(line) {
            return false;
        }
        self.file_emitted = true;
        true
    }
}

impl Sink for MatchSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.signal.is_cancelled() {
            self.state.cancelled = true;
            return Ok(false);
        }
        if self.state.hit_match_limit {
            return Ok(false);
        }
        let line_no = mat.line_number().unwrap_or(0);
        let text = format_line_bytes(mat.bytes());
        let formatted = format!("{}:{line_no}: {text}", self.display_path);
        if !self.push_entry_line(&formatted) {
            return Ok(false);
        }
        self.state.match_count += 1;
        if self.state.match_count >= self.match_limit {
            self.state.hit_match_limit = true;
            self.remaining_after_context = self.context_lines;
            return Ok(self.context_lines > 0);
        }
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.signal.is_cancelled() {
            self.state.cancelled = true;
            return Ok(false);
        }
        if self.state.hit_match_limit {
            if *ctx.kind() != SinkContextKind::After || self.remaining_after_context == 0 {
                return Ok(false);
            }
            let line_no = ctx.line_number().unwrap_or(0);
            let text = format_line_bytes(ctx.bytes());
            let formatted = format!("{}-{line_no}- {text}", self.display_path);
            if !self.push_entry_line(&formatted) {
                return Ok(false);
            }
            self.remaining_after_context -= 1;
            return Ok(self.remaining_after_context > 0);
        }
        let line_no = ctx.line_number().unwrap_or(0);
        let text = format_line_bytes(ctx.bytes());
        let formatted = format!("{}-{line_no}- {text}", self.display_path);
        Ok(self.push_entry_line(&formatted))
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if self.signal.is_cancelled() {
            self.state.cancelled = true;
            return Ok(false);
        }
        if self.state.hit_match_limit {
            return Ok(false);
        }
        if self.context_lines > 0 && self.file_emitted {
            return Ok(self.push_raw_line("--"));
        }
        Ok(true)
    }
}

fn format_line_bytes(raw: &[u8]) -> Cow<'_, str> {
    let stripped = raw.strip_suffix(b"\n").unwrap_or(raw);
    let stripped = stripped.strip_suffix(b"\r").unwrap_or(stripped);
    let text = String::from_utf8_lossy(stripped);
    match text.char_indices().nth(MAX_LINE_CHARS) {
        Some((byte_idx, _)) => Cow::Owned(format!("{}…[truncated]", &text[..byte_idx])),
        None => text,
    }
}
