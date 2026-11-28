use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use ignore::{ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::registry::{
    ABORTED_MESSAGE, DEFAULT_MAX_BYTES, GLOB_TOOL_NAME, ToolExecution, default_max_kb, error_result,
};

const DEFAULT_GLOB_LIMIT: usize = 100;
const MAX_GLOB_LIMIT: usize = 1000;
const MAX_COLLECT_MATCHES: usize = 10_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GlobArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    pub(crate) limit: Option<u64>,
}

impl GlobArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.pattern.trim().is_empty() {
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
        name: GLOB_TOOL_NAME.to_string(),
        description: format!(
            "Find files matching a glob pattern, respecting .gitignore and skipping .git. Patterns without path separators match filenames at any depth (e.g. '*.rs'). Results are sorted by newest modification time first, capped at {DEFAULT_GLOB_LIMIT} by default (max {MAX_GLOB_LIMIT}) and {}KB.",
            default_max_kb()
        ),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g. '*.rs', 'src/**/*.ts', '.github/**')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (relative to workspace or absolute; defaults to workspace root)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_GLOB_LIMIT,
                    "description": format!("Maximum number of file paths to return (default {DEFAULT_GLOB_LIMIT}, max {MAX_GLOB_LIMIT})")
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

pub(super) struct CompiledGlob {
    pub(super) matcher: GlobMatcher,
    pub(super) static_prefix: Vec<String>,
    pub(super) include_hidden: bool,
}

pub(super) fn compile_glob(raw_pattern: &str) -> Result<CompiledGlob, ToolExecution> {
    let has_sep = raw_pattern.contains('/') || raw_pattern.contains('\\');
    let mut normalized = raw_pattern.replace('\\', "/");
    loop {
        if let Some(rest) = normalized.strip_prefix("./") {
            normalized = rest.to_owned();
        } else if let Some(rest) = normalized.strip_prefix('/') {
            normalized = rest.to_owned();
        } else {
            break;
        }
    }
    if normalized.is_empty() {
        return Err(error_result("invalid tool arguments: pattern must not be empty"));
    }
    let effective = if has_sep {
        normalized
    } else {
        format!("**/{normalized}")
    };
    let include_hidden = pattern_targets_hidden(&effective);
    let matcher = GlobBuilder::new(&effective)
        .literal_separator(true)
        .build()
        .map_err(|error| error_result(format!("Invalid glob pattern: {error}")))?
        .compile_matcher();
    let static_prefix = if has_sep {
        extract_static_prefix(&effective)
    } else {
        Vec::new()
    };
    Ok(CompiledGlob { matcher, static_prefix, include_hidden })
}

fn extract_static_prefix(pattern: &str) -> Vec<String> {
    let segments: Vec<&str> = pattern.split('/').collect();
    if segments.len() < 2 || segments.contains(&"..") {
        return Vec::new();
    }
    segments[..segments.len() - 1]
        .iter()
        .take_while(|segment| {
            !segment.is_empty() && **segment != "." && !segment.contains(['*', '?', '[', ']', '{', '}'])
        })
        .map(|segment| (*segment).to_owned())
        .collect()
}

pub(super) fn walker_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |count| count.get().clamp(2, 8))
}

pub(super) fn pattern_targets_hidden(pattern: &str) -> bool {
    pattern
        .split('/')
        .any(|segment| segment.starts_with('.') && segment != "." && segment != "..")
}

pub(super) enum SearchTarget {
    Skip,
    Ready(PathBuf, std::fs::Metadata),
}

fn is_git_dir_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|s| s.eq_ignore_ascii_case(".git"))
}

fn has_git_component(path: &Path) -> bool {
    path.components().any(|component| is_git_dir_name(component.as_os_str()))
}

pub(super) fn inspect_search_target(path: Option<&str>, cwd: &Path) -> Result<SearchTarget, ToolExecution> {
    let explicit = path.map(str::trim).filter(|p| !p.is_empty() && *p != ".");
    let search_path = match explicit {
        Some(raw) => cwd.join(raw),
        None => cwd.to_path_buf(),
    };
    if let Some(raw) = explicit {
        let targets_git = has_git_component(Path::new(raw))
            || search_path.strip_prefix(cwd).ok().is_some_and(has_git_component);
        if targets_git {
            return Ok(SearchTarget::Skip);
        }
    }
    let meta = std::fs::metadata(&search_path).map_err(|error| {
        let shown = path.unwrap_or(".");
        error_result(format!("Could not access path: {shown}. {error}"))
    })?;
    Ok(SearchTarget::Ready(search_path, meta))
}

pub(super) fn format_display_path(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => app_core::display_path(path),
    }
}

pub(super) fn build_walker(
    root: &Path,
    include_hidden: bool,
    sort_by_name: bool,
    static_prefix: &[String],
) -> WalkBuilder {
    let prefix = static_prefix.to_vec();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .ignore(true)
        .follow_links(false)
        .filter_entry(move |entry| {
            if is_git_dir_name(entry.file_name()) {
                return false;
            }
            let depth = entry.depth();
            if depth > 0 && depth <= prefix.len() {
                if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    return false;
                }
                return entry.file_name().to_str() == Some(prefix[depth - 1].as_str());
            }
            true
        });
    if sort_by_name {
        builder.sort_by_file_name(Ord::cmp);
    }
    builder
}

pub(crate) fn execute(args: &GlobArgs, cwd: &Path, signal: &CancellationToken) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let compiled = match compile_glob(&args.pattern) {
        Ok(compiled) => compiled,
        Err(error) => return error,
    };
    let limit = args.limit.map_or(DEFAULT_GLOB_LIMIT, |value| {
        usize::try_from(value).unwrap_or(MAX_GLOB_LIMIT).clamp(1, MAX_GLOB_LIMIT)
    });
    let (search_root, metadata) = match inspect_search_target(args.path.as_deref(), cwd) {
        Ok(SearchTarget::Skip) => return ToolExecution::text("No files found."),
        Ok(SearchTarget::Ready(path, metadata)) => (path, metadata),
        Err(error) => return error,
    };
    if !metadata.is_dir() {
        let shown = args.path.as_deref().unwrap_or(".");
        return error_result(format!("Path is not a directory: {shown}"));
    }

    let shared_matches = Mutex::new(Vec::<(SystemTime, String)>::new());
    let collected = AtomicUsize::new(0);
    let hit_collect_cap = AtomicBool::new(false);
    let mut builder = GlobVisitorBuilder {
        walk_root: &search_root,
        cwd,
        matcher: &compiled.matcher,
        signal,
        shared_matches: &shared_matches,
        collected: &collected,
        hit_collect_cap: &hit_collect_cap,
    };
    build_walker(&search_root, compiled.include_hidden, false, &compiled.static_prefix)
        .threads(walker_threads())
        .build_parallel()
        .visit(&mut builder);

    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let mut matches = shared_matches.into_inner().expect("glob results mutex poisoned");
    let hit_collect_cap = hit_collect_cap.load(Ordering::Relaxed);
    if matches.len() > MAX_COLLECT_MATCHES {
        matches.truncate(MAX_COLLECT_MATCHES);
    }
    if matches.is_empty() {
        return ToolExecution::text("No files found.");
    }

    matches.sort_by(|(a_time, a_path), (b_time, b_path)| {
        Reverse(*a_time).cmp(&Reverse(*b_time)).then_with(|| a_path.cmp(b_path))
    });

    let total_found = matches.len();
    let mut output = String::new();
    let mut shown = 0usize;
    let mut truncated_by_bytes = false;

    for (_, path) in matches.iter().take(limit) {
        let extra = path.len() + usize::from(!output.is_empty());
        if output.len() + extra > DEFAULT_MAX_BYTES {
            truncated_by_bytes = true;
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(path);
        shown += 1;
    }

    if truncated_by_bytes {
        let _ = write!(
            output,
            "\n\n[Output truncated at {}KB (showing {shown} of {total_found} matches). Narrow the path or pattern.]",
            default_max_kb()
        );
    } else if hit_collect_cap {
        let _ = write!(
            output,
            "\n\n[Stopped scanning after {MAX_COLLECT_MATCHES} matching files; results may not be globally sorted by mtime (showing {shown}). Narrow the path or pattern.]"
        );
    } else if total_found > shown {
        let _ = write!(
            output,
            "\n\n[Showing {shown} of {total_found} matching files (sorted by newest mtime). Narrow the path or pattern, or increase limit.]"
        );
    }

    ToolExecution::text(output)
}

struct GlobVisitorBuilder<'a> {
    walk_root: &'a Path,
    cwd: &'a Path,
    matcher: &'a GlobMatcher,
    signal: &'a CancellationToken,
    shared_matches: &'a Mutex<Vec<(SystemTime, String)>>,
    collected: &'a AtomicUsize,
    hit_collect_cap: &'a AtomicBool,
}

impl<'s> ParallelVisitorBuilder<'s> for GlobVisitorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(GlobVisitor {
            walk_root: self.walk_root,
            cwd: self.cwd,
            matcher: self.matcher,
            signal: self.signal,
            shared_matches: self.shared_matches,
            collected: self.collected,
            hit_collect_cap: self.hit_collect_cap,
            local: Vec::new(),
        })
    }
}

struct GlobVisitor<'a> {
    walk_root: &'a Path,
    cwd: &'a Path,
    matcher: &'a GlobMatcher,
    signal: &'a CancellationToken,
    shared_matches: &'a Mutex<Vec<(SystemTime, String)>>,
    collected: &'a AtomicUsize,
    hit_collect_cap: &'a AtomicBool,
    local: Vec<(SystemTime, String)>,
}

impl Drop for GlobVisitor<'_> {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.shared_matches.lock().expect("glob results mutex poisoned").append(&mut self.local);
        }
    }
}

impl ParallelVisitor for GlobVisitor<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        if self.signal.is_cancelled() || self.hit_collect_cap.load(Ordering::Relaxed) {
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            return WalkState::Continue;
        }
        let Ok(rel) = entry.path().strip_prefix(self.walk_root) else {
            return WalkState::Continue;
        };
        let rel_slash = rel.to_string_lossy().replace('\\', "/");
        if !self.matcher.is_match(&rel_slash) {
            return WalkState::Continue;
        }
        let prev = self.collected.fetch_add(1, Ordering::Relaxed);
        if prev >= MAX_COLLECT_MATCHES {
            self.hit_collect_cap.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let display = format_display_path(entry.path(), self.cwd);
        self.local.push((mtime, display));
        if prev + 1 >= MAX_COLLECT_MATCHES {
            self.hit_collect_cap.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        WalkState::Continue
    }
}
