use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use globset::GlobMatcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::{ParallelVisitor, ParallelVisitorBuilder, WalkState};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::glob::{
    SearchTarget, build_walker, compile_glob, format_display_path, inspect_search_target, walker_threads,
};
use super::registry::{
    ABORTED_MESSAGE, DEFAULT_MAX_BYTES, GREP_TOOL_NAME, ToolExecution, default_max_kb, error_result,
};

const DEFAULT_GREP_LIMIT: usize = 100;
const MAX_GREP_LIMIT: usize = 500;
const MAX_GREP_CONTEXT: usize = 10;
const MAX_LINE_CHARS: usize = 500;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepOutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

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
    #[serde(default)]
    pub(crate) output_mode: GrepOutputMode,
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

pub(crate) fn spec() -> model::ToolDefinition {
    model::ToolDefinition {
        name: GREP_TOOL_NAME.to_string(),
        description: format!(
            "Search file contents using regex (including multiline patterns) or literal matching, respecting .gitignore and skipping binary files and .git. Supports output_mode: 'content' (default, 'path:line: content'), 'files_with_matches' (matching file paths only), or 'count' ('path:count'). Capped at {DEFAULT_GREP_LIMIT} matches/files by default (max {MAX_GREP_LIMIT}), {MAX_LINE_CHARS} chars per line, and {}KB total.",
            default_max_kb()
        ),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern (supports multiline '\\n' patterns) or literal text when literal=true" },
                "path": { "type": "string", "description": "File or directory to search (relative to workspace or absolute; defaults to workspace root)" },
                "glob": { "type": "string", "description": "Optional glob to filter searched files (e.g. '*.rs', 'src/**/*.ts')" },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive matching (default false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as a literal string rather than regex (default false)" },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode: 'content' (default matching lines), 'files_with_matches' (matching file paths only), or 'count' (matching line counts per file)"
                },
                "context": { "type": "integer", "minimum": 0, "maximum": MAX_GREP_CONTEXT, "description": format!("Context lines before/after each match in content mode (default 0, max {MAX_GREP_CONTEXT})") },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_GREP_LIMIT, "description": format!("Max matching lines (or files in files_with_matches/count mode) to return (default {DEFAULT_GREP_LIMIT}, max {MAX_GREP_LIMIT})") }
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
    let context_lines = if args.output_mode == GrepOutputMode::Content {
        args.context
            .map_or(0, |value| usize::try_from(value).unwrap_or(MAX_GREP_CONTEXT).min(MAX_GREP_CONTEXT))
    } else {
        0
    };
    let limit = args.limit.map_or(DEFAULT_GREP_LIMIT, |value| {
        usize::try_from(value).unwrap_or(MAX_GREP_LIMIT).clamp(1, MAX_GREP_LIMIT)
    });

    let (search_path, metadata) = match inspect_search_target(args.path.as_deref(), cwd) {
        Ok(SearchTarget::Skip) => return ToolExecution::text("No matches found."),
        Ok(SearchTarget::Ready(path, metadata)) => (path, metadata),
        Err(error) => return error,
    };

    let mut file_hits = Vec::new();
    let mut read_errors = 0usize;
    let mut first_read_error: Option<(String, String)> = None;
    let mut stopped_early = false;

    let opts = FileSearchOpts {
        matcher: &matcher,
        mode: args.output_mode,
        context_lines,
        match_limit: limit,
        signal,
    };
    if metadata.is_file() {
        let display = format_display_path(&search_path, cwd);
        let mut searcher = build_searcher(context_lines);
        match search_single_file(&mut searcher, &opts, &search_path, &display) {
            Ok(Some(hit)) => file_hits.push(hit),
            Ok(None) => {}
            Err(error) => {
                if signal.is_cancelled() {
                    return error_result(ABORTED_MESSAGE);
                }
                let shown = args.path.as_deref().unwrap_or(&display);
                return error_result(format!("Could not search file: {shown}. {error}"));
            }
        }
    } else if metadata.is_dir() {
        let include_hidden = file_glob.as_ref().is_some_and(|g| g.include_hidden);
        let static_prefix = file_glob.as_ref().map_or(&[][..], |g| g.static_prefix.as_slice());
        let active_matcher = file_glob.as_ref().map(|g| &g.matcher);
        let shared = Mutex::new(CollectedSearch::default());
        let parallel_cap = limit.saturating_mul(4).max(400);
        let total_units = AtomicUsize::new(0);
        let stop_flag = AtomicBool::new(false);
        let run = GrepRun {
            walk_root: &search_path,
            cwd,
            opts,
            file_matcher: active_matcher,
            parallel_cap,
            total_units: &total_units,
            stop_flag: &stop_flag,
            shared: &shared,
        };
        let mut builder = GrepVisitorBuilder { run: &run };
        build_walker(&search_path, include_hidden, false, static_prefix)
            .threads(walker_threads())
            .build_parallel()
            .visit(&mut builder);

        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let collected = shared.into_inner().expect("grep results mutex poisoned");
        file_hits = collected.hits;
        read_errors = collected.read_errors;
        first_read_error = collected.first_read_error;
        stopped_early = stop_flag.load(Ordering::Relaxed);
    } else {
        let shown = args.path.as_deref().unwrap_or(".");
        return error_result(format!("Path is neither a file nor a directory: {shown}"));
    }

    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if file_hits.is_empty() {
        if let Some((path, err)) = first_read_error {
            return error_result(format!("Could not search file: {path}. {err}"));
        }
        return ToolExecution::text("No matches found.");
    }

    file_hits.sort_by(|left, right| left.display.cmp(&right.display));
    let mut output = String::new();
    let mut emitted_units = 0usize;
    let mut hit_match_limit = false;
    let mut hit_byte_limit = false;

    for hit in file_hits {
        if emitted_units >= limit {
            hit_match_limit = true;
            break;
        }
        match args.output_mode {
            GrepOutputMode::FilesWithMatches => {
                if !push_bounded_line(&mut output, &hit.display) {
                    hit_byte_limit = true;
                    break;
                }
                emitted_units += 1;
            }
            GrepOutputMode::Count => {
                let line = format!("{}:{}", hit.display, hit.match_count);
                if !push_bounded_line(&mut output, &line) {
                    hit_byte_limit = true;
                    break;
                }
                emitted_units += 1;
            }
            GrepOutputMode::Content => {
                if !output.is_empty() && context_lines > 0 && !push_bounded_line(&mut output, "--") {
                    hit_byte_limit = true;
                    break;
                }
                let mut file_hit_limit = false;
                let mut remaining_after = 0usize;
                for entry in hit.entries {
                    if file_hit_limit {
                        if !entry.is_after_context || remaining_after == 0 {
                            break;
                        }
                        if !push_bounded_line(&mut output, &entry.text) {
                            hit_byte_limit = true;
                            break;
                        }
                        remaining_after -= 1;
                        continue;
                    }
                    if !push_bounded_line(&mut output, &entry.text) {
                        hit_byte_limit = true;
                        break;
                    }
                    if entry.is_match {
                        emitted_units += 1;
                        if emitted_units >= limit {
                            hit_match_limit = true;
                            file_hit_limit = true;
                            remaining_after = context_lines;
                            if context_lines == 0 {
                                break;
                            }
                        }
                    }
                }
                if !hit_match_limit && hit.hit_byte_limit {
                    hit_byte_limit = true;
                }
                if hit_byte_limit || hit_match_limit {
                    break;
                }
            }
        }
    }

    if hit_byte_limit {
        let _ = write!(
            output,
            "\n\n[Output truncated at {}KB after {emitted_units} matches. Narrow the path or glob filter.]",
            default_max_kb()
        );
    } else if hit_match_limit || stopped_early {
        let _ = write!(
            output,
            "\n\n[Stopped at match limit ({limit}). Narrow the path or glob filter, or increase limit.]"
        );
    }
    if let Some((path, err)) = first_read_error {
        let _ = write!(output, "\n\n[Note: skipped {read_errors} unreadable file(s); first: {path}: {err}]");
    }

    ToolExecution::text(output)
}

fn build_searcher(context_lines: usize) -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .multi_line(true)
        .before_context(context_lines)
        .after_context(context_lines)
        .build()
}

fn push_bounded_line(output: &mut String, line: &str) -> bool {
    let sep = usize::from(!output.is_empty());
    if output.len() + sep + line.len() > DEFAULT_MAX_BYTES {
        return false;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(line);
    true
}

struct ContentEntry {
    text: String,
    is_match: bool,
    is_after_context: bool,
}

struct FileHit {
    display: String,
    match_count: usize,
    entries: Vec<ContentEntry>,
    hit_byte_limit: bool,
}

#[derive(Default)]
struct CollectedSearch {
    hits: Vec<FileHit>,
    read_errors: usize,
    first_read_error: Option<(String, String)>,
}

struct FileSearchOpts<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    mode: GrepOutputMode,
    context_lines: usize,
    match_limit: usize,
    signal: &'a CancellationToken,
}

struct GrepRun<'a> {
    walk_root: &'a Path,
    cwd: &'a Path,
    opts: FileSearchOpts<'a>,
    file_matcher: Option<&'a GlobMatcher>,
    parallel_cap: usize,
    total_units: &'a AtomicUsize,
    stop_flag: &'a AtomicBool,
    shared: &'a Mutex<CollectedSearch>,
}

struct GrepVisitorBuilder<'a> {
    run: &'a GrepRun<'a>,
}

impl<'s> ParallelVisitorBuilder<'s> for GrepVisitorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(GrepVisitor {
            run: self.run,
            searcher: build_searcher(self.run.opts.context_lines),
            local: CollectedSearch::default(),
        })
    }
}

struct GrepVisitor<'a> {
    run: &'a GrepRun<'a>,
    searcher: Searcher,
    local: CollectedSearch,
}

impl Drop for GrepVisitor<'_> {
    fn drop(&mut self) {
        if self.local.hits.is_empty() && self.local.read_errors == 0 {
            return;
        }
        let mut shared = self.run.shared.lock().expect("grep results mutex poisoned");
        shared.hits.append(&mut self.local.hits);
        shared.read_errors += self.local.read_errors;
        if shared.first_read_error.is_none() {
            shared.first_read_error = self.local.first_read_error.take();
        }
    }
}

impl ParallelVisitor for GrepVisitor<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        let run = self.run;
        if run.opts.signal.is_cancelled() || run.stop_flag.load(Ordering::Relaxed) {
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return WalkState::Continue;
        }
        if let Some(matcher) = run.file_matcher {
            let Ok(rel) = entry.path().strip_prefix(run.walk_root) else {
                return WalkState::Continue;
            };
            let rel_slash = rel.to_string_lossy().replace('\\', "/");
            if !matcher.is_match(&rel_slash) {
                return WalkState::Continue;
            }
        }
        let display = format_display_path(entry.path(), run.cwd);
        match search_single_file(&mut self.searcher, &run.opts, entry.path(), &display) {
            Ok(Some(hit)) => {
                let added = match run.opts.mode {
                    GrepOutputMode::Content => hit.match_count,
                    GrepOutputMode::FilesWithMatches | GrepOutputMode::Count => 1,
                };
                self.local.hits.push(hit);
                if run.total_units.fetch_add(added, Ordering::Relaxed) + added >= run.parallel_cap {
                    run.stop_flag.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }
            Ok(None) => {}
            Err(error) => {
                if run.opts.signal.is_cancelled() {
                    return WalkState::Quit;
                }
                self.local.read_errors += 1;
                if self.local.first_read_error.is_none() {
                    self.local.first_read_error = Some((display, error.to_string()));
                }
            }
        }
        WalkState::Continue
    }
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

fn search_single_file(
    searcher: &mut Searcher,
    opts: &FileSearchOpts<'_>,
    path: &Path,
    display_path: &str,
) -> io::Result<Option<FileHit>> {
    if opts.signal.is_cancelled() {
        return Err(io::Error::other(ABORTED_MESSAGE));
    }
    let file = std::fs::File::open(path)?;
    let mut sink = FileSink {
        display_path,
        opts,
        entries: Vec::new(),
        bytes: 0,
        match_count: 0,
        hit_limit: false,
        hit_byte_limit: false,
        remaining_after: 0,
    };
    searcher.search_reader(
        opts.matcher,
        CancellableReader { inner: file, signal: opts.signal },
        &mut sink,
    )?;
    if sink.match_count == 0 && !sink.hit_byte_limit {
        return Ok(None);
    }
    Ok(Some(FileHit {
        display: display_path.to_owned(),
        match_count: sink.match_count,
        entries: sink.entries,
        hit_byte_limit: sink.hit_byte_limit,
    }))
}

struct FileSink<'a> {
    display_path: &'a str,
    opts: &'a FileSearchOpts<'a>,
    entries: Vec<ContentEntry>,
    bytes: usize,
    match_count: usize,
    hit_limit: bool,
    hit_byte_limit: bool,
    remaining_after: usize,
}

impl FileSink<'_> {
    fn push_entry(&mut self, text: String, is_match: bool, is_after_context: bool) -> bool {
        let extra = text.len() + usize::from(!self.entries.is_empty());
        if self.bytes + extra > DEFAULT_MAX_BYTES {
            self.hit_byte_limit = true;
            return false;
        }
        self.bytes += extra;
        self.entries.push(ContentEntry { text, is_match, is_after_context });
        true
    }
}

impl Sink for FileSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.opts.signal.is_cancelled() {
            return Ok(false);
        }
        let raw = mat.bytes().strip_suffix(b"\n").unwrap_or(mat.bytes());
        match self.opts.mode {
            GrepOutputMode::FilesWithMatches => {
                self.match_count = 1;
                return Ok(false);
            }
            GrepOutputMode::Count => {
                self.match_count += raw.split(|byte| *byte == b'\n').count();
                return Ok(true);
            }
            GrepOutputMode::Content => {}
        }
        if self.hit_limit {
            return Ok(false);
        }
        let start_line = mat.line_number().unwrap_or(0);
        for (offset, segment) in raw.split(|byte| *byte == b'\n').enumerate() {
            let line_no = start_line.saturating_add(offset as u64);
            let formatted = format!("{}:{line_no}: {}", self.display_path, format_line_bytes(segment));
            if !self.push_entry(formatted, true, false) {
                return Ok(false);
            }
            self.match_count += 1;
            if self.match_count >= self.opts.match_limit {
                self.hit_limit = true;
                self.remaining_after = self.opts.context_lines;
                return Ok(self.opts.context_lines > 0);
            }
        }
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.opts.signal.is_cancelled() || self.opts.mode != GrepOutputMode::Content {
            return Ok(false);
        }
        let is_after = *ctx.kind() == SinkContextKind::After;
        if self.hit_limit && (!is_after || self.remaining_after == 0) {
            return Ok(false);
        }
        let formatted = format!(
            "{}-{}- {}",
            self.display_path,
            ctx.line_number().unwrap_or(0),
            format_line_bytes(ctx.bytes())
        );
        if !self.push_entry(formatted, false, is_after) {
            return Ok(false);
        }
        if self.hit_limit {
            self.remaining_after -= 1;
            return Ok(self.remaining_after > 0);
        }
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        if self.opts.signal.is_cancelled() || self.opts.mode != GrepOutputMode::Content || self.hit_limit {
            return Ok(false);
        }
        Ok(self.opts.context_lines == 0
            || self.entries.is_empty()
            || self.push_entry("--".to_owned(), false, false))
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
