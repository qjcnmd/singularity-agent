use std::cmp::Reverse;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::registry::{ABORTED_MESSAGE, DEFAULT_MAX_BYTES, ToolExecution, default_max_kb, error_result};

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

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: "glob".to_string(),
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
    Ok(CompiledGlob { matcher, include_hidden })
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

fn has_git_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_str().is_some_and(|s| s.eq_ignore_ascii_case(".git")))
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

pub(super) fn build_walker(root: &Path, include_hidden: bool, sort_by_name: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!include_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .ignore(true)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git");
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

    let mut matches: Vec<(SystemTime, String)> = Vec::new();
    let mut hit_collect_cap = false;

    for entry in build_walker(&search_root, compiled.include_hidden, false).build() {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(&search_root) else {
            continue;
        };
        let rel_slash = rel.to_string_lossy().replace('\\', "/");
        if !compiled.matcher.is_match(&rel_slash) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let display = format_display_path(entry.path(), cwd);
        matches.push((mtime, display));
        if matches.len() >= MAX_COLLECT_MATCHES {
            hit_collect_cap = true;
            break;
        }
    }

    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
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
