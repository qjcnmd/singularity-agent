use crate::absolute_path::AbsolutePathBuf;
use crate::path_normalization::lexical_path_key;
use crate::path_safety::ProtectedMetadataError;
use crate::path_safety::ensure_case_insensitive_directory_path;
use crate::path_safety::ensure_case_insensitive_path_ancestors;
use crate::permissions::FileSystemAccessMode;
use crate::permissions::FileSystemPath;
use crate::permissions::FileSystemSandboxEntry;
use crate::permissions::FileSystemSandboxPolicy;
use crate::permissions::ReadDenyMatcher;
use std::collections::HashSet;
use std::fs::Metadata;
use std::path::Path;
use std::path::PathBuf;

const MAX_DENY_READ_SCAN_DEPTH: usize = 64;
const MAX_DENY_READ_SCAN_ENTRIES: usize = 100_000;

struct GlobScanPlan {
    root: PathBuf,
    max_depth: usize,
    recursive: bool,
}

struct ScanState {
    scanned_entries: usize,
}

struct GlobScanContext<'a> {
    matcher: &'a ReadDenyMatcher,
    paths: &'a mut Vec<AbsolutePathBuf>,
    seen_paths: &'a mut HashSet<PathBuf>,
    seen_scan_dirs: &'a mut HashSet<String>,
    scan_boundary: &'a Path,
    scan_plan: &'a GlobScanPlan,
    scan_state: &'a mut ScanState,
}

/// 将 filesystem deny entries 展开为 Windows ACL 可应用的具体路径。
///
/// Windows ACL 不直接理解 glob；exact 路径原样保留，glob 只对现有条目做一次有界快照
/// 展开。无法安全检查的子树会把其词法路径加入 deny-read，不静默放行；reparse target
/// 仅在 workspace 边界内用于有界扫描，最终 ACL preflight 对 reparse 路径 fail closed。
pub fn resolve_windows_deny_read_paths(
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    cwd: &AbsolutePathBuf,
) -> Result<Vec<AbsolutePathBuf>, String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for path in file_system_sandbox_policy.get_unreadable_roots_with_cwd(cwd.as_path()) {
        push_absolute_path(&mut paths, &mut seen, path.into_path_buf())?;
    }

    let unreadable_globs = file_system_sandbox_policy.get_unreadable_globs_with_cwd(cwd.as_path());
    if unreadable_globs.is_empty() {
        return Ok(paths);
    }

    let glob_policy = FileSystemSandboxPolicy::restricted(
        unreadable_globs
            .iter()
            .map(|pattern| FileSystemSandboxEntry {
                path: FileSystemPath::GlobPattern {
                    pattern: pattern.clone(),
                },
                access: FileSystemAccessMode::Deny,
            })
            .collect(),
    );
    let Some(matcher) = ReadDenyMatcher::try_new(&glob_policy, cwd.as_path())
        .map_err(|error| sanitize_glob_error(&error))?
    else {
        return Ok(paths);
    };

    let scan_boundary = dunce::canonicalize(cwd.as_path())
        .map_err(|_| "deny_read_resolution_workspace_unavailable".to_string())?;
    ensure_case_insensitive_directory_path(&scan_boundary)
        .map_err(|error| sanitize_path_safety_error(&error))?;
    let mut scan_plans = Vec::new();
    for pattern in unreadable_globs {
        let mut scan_plan =
            glob_scan_plan(&pattern, file_system_sandbox_policy.glob_scan_max_depth);
        if !scan_plan.root.is_absolute() {
            scan_plan.root = cwd.as_path().join(&scan_plan.root);
        }
        ensure_case_insensitive_path_ancestors(&scan_plan.root)
            .map_err(|error| sanitize_path_safety_error(&error))?;
        merge_scan_plan(&mut scan_plans, scan_plan);
    }

    let mut scan_state = ScanState { scanned_entries: 0 };
    for scan_plan in scan_plans {
        let mut seen_scan_dirs = HashSet::new();
        let mut context = GlobScanContext {
            matcher: &matcher,
            paths: &mut paths,
            seen_paths: &mut seen,
            seen_scan_dirs: &mut seen_scan_dirs,
            scan_boundary: &scan_boundary,
            scan_plan: &scan_plan,
            scan_state: &mut scan_state,
        };
        collect_existing_glob_matches(&scan_plan.root, /*depth*/ 0, &mut context)?;
    }

    Ok(paths)
}

fn collect_existing_glob_matches(
    path: &Path,
    depth: usize,
    context: &mut GlobScanContext<'_>,
) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
            return Ok(());
        }
    };
    let reparse_point = metadata_is_reparse_point(&metadata);

    if context.matcher.is_read_denied(path) {
        push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
        if metadata.is_dir() || reparse_point {
            return Ok(());
        }
    };

    let canonical = match dunce::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
            return Ok(());
        }
    };
    ensure_case_insensitive_path_ancestors(&canonical)
        .map_err(|error| sanitize_path_safety_error(&error))?;
    if !path_is_within(&canonical, context.scan_boundary) {
        // workspace 外的 reparse target 是未知子树：只 deny 词法入口，不跟随越界 target。
        push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
        return Ok(());
    }
    if context.matcher.is_read_denied(&canonical) {
        push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
        if metadata.is_dir() || reparse_point {
            return Ok(());
        }
    }

    let target_metadata = if reparse_point {
        match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => {
                push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
                return Ok(());
            }
        }
    } else {
        metadata
    };
    if !target_metadata.is_dir() {
        return Ok(());
    }
    ensure_case_insensitive_directory_path(&canonical)
        .map_err(|error| sanitize_path_safety_error(&error))?;

    // canonical directory key 防止 symlink/junction cycle 无限递归；词法路径交给
    // ACL planner，并由其统一拒绝无法 no-follow enforcement 的 reparse 路径。
    let scan_key = path_key(&canonical);
    if !context.seen_scan_dirs.insert(scan_key) {
        return Ok(());
    }

    if depth >= context.scan_plan.max_depth {
        if context.scan_plan.recursive {
            push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
        }
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
            return Ok(());
        }
    } {
        if context.scan_state.scanned_entries >= MAX_DENY_READ_SCAN_ENTRIES {
            push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
            return Ok(());
        }
        context.scan_state.scanned_entries += 1;
        match entry {
            Ok(entry) => entries.push(entry),
            Err(_) => {
                push_absolute_path(context.paths, context.seen_paths, path.to_path_buf())?;
                return Ok(());
            }
        }
    }
    entries.sort_by_key(|entry| path_key(&entry.path()));
    for entry in entries {
        collect_existing_glob_matches(&entry.path(), depth + 1, context)?;
    }

    Ok(())
}

fn push_absolute_path(
    paths: &mut Vec<AbsolutePathBuf>,
    seen: &mut HashSet<PathBuf>,
    path: PathBuf,
) -> Result<(), String> {
    let absolute_path = AbsolutePathBuf::from_absolute_path(dunce::simplified(&path))
        .map_err(|_| "deny_read_resolution_invalid_path".to_string())?;
    if seen.insert(absolute_path.to_path_buf()) {
        paths.push(absolute_path);
    }
    Ok(())
}

fn glob_scan_plan(pattern: &str, configured_max_depth: Option<usize>) -> GlobScanPlan {
    // 从第一个 glob 元字符前的最深词法目录开始扫描；例如 `C:\repo\**\*.env`
    // 只扫描 `C:\repo`，避免退回当前目录或盘符根目录。
    let first_glob = pattern
        .char_indices()
        .find(|(_, ch)| matches!(ch, '*' | '?' | '['))
        .map(|(index, _)| index)
        .unwrap_or(pattern.len());
    let literal_prefix = &pattern[..first_glob];
    let Some(separator_index) = literal_prefix.rfind(['/', '\\']) else {
        return GlobScanPlan {
            root: PathBuf::from("."),
            max_depth: effective_glob_scan_max_depth(pattern, configured_max_depth),
            recursive: pattern
                .split(['/', '\\'])
                .any(|component| component == "**"),
        };
    };
    let pattern_suffix = &pattern[separator_index + 1..];
    let is_drive_root_separator = separator_index > 0
        && literal_prefix
            .as_bytes()
            .get(separator_index - 1)
            .is_some_and(|ch| *ch == b':');
    if separator_index == 0 || is_drive_root_separator {
        return GlobScanPlan {
            root: PathBuf::from(&literal_prefix[..=separator_index]),
            max_depth: effective_glob_scan_max_depth(pattern_suffix, configured_max_depth),
            recursive: pattern_suffix
                .split(['/', '\\'])
                .any(|component| component == "**"),
        };
    }
    GlobScanPlan {
        root: PathBuf::from(literal_prefix[..separator_index].to_string()),
        max_depth: effective_glob_scan_max_depth(pattern_suffix, configured_max_depth),
        recursive: pattern_suffix
            .split(['/', '\\'])
            .any(|component| component == "**"),
    }
}

fn effective_glob_scan_max_depth(
    pattern_suffix: &str,
    configured_max_depth: Option<usize>,
) -> usize {
    let components = pattern_suffix
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components.contains(&"**") {
        return configured_max_depth
            .unwrap_or(MAX_DENY_READ_SCAN_DEPTH)
            .min(MAX_DENY_READ_SCAN_DEPTH);
    }
    configured_max_depth
        .map_or(components.len(), |max_depth| {
            max_depth.min(components.len())
        })
        .min(MAX_DENY_READ_SCAN_DEPTH)
}

fn merge_scan_plan(plans: &mut Vec<GlobScanPlan>, candidate: GlobScanPlan) {
    if let Some(existing) = plans
        .iter_mut()
        .find(|plan| path_key(&plan.root) == path_key(&candidate.root))
    {
        existing.max_depth = existing.max_depth.max(candidate.max_depth);
        existing.recursive |= candidate.recursive;
    } else {
        plans.push(candidate);
    }
}

fn sanitize_glob_error(error: &str) -> String {
    let reason = if error.contains("invalid range") {
        "invalid range"
    } else {
        "invalid pattern"
    };
    format!("invalid deny-read glob pattern: {reason}")
}

fn sanitize_path_safety_error(error: &anyhow::Error) -> String {
    match error.downcast_ref::<ProtectedMetadataError>() {
        Some(ProtectedMetadataError::CaseSensitiveDirectoryUnsupported { .. }) => {
            "deny_read_resolution_case_sensitive_directory".to_string()
        }
        Some(ProtectedMetadataError::CaseSensitivityQueryFailed { .. }) => {
            "deny_read_resolution_case_sensitivity_query_failed".to_string()
        }
        _ => "deny_read_resolution_path_validation_failed".to_string(),
    }
}

fn path_key(path: &Path) -> String {
    lexical_path_key(path)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path = path_key(path);
        let root = path_key(root);
        path == root || path.starts_with(&format!("{root}/"))
    } else {
        path.starts_with(root)
    }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::glob_scan_plan;
    use super::path_is_within;
    use super::resolve_windows_deny_read_paths;
    use super::sanitize_path_safety_error;
    use crate::absolute_path::AbsolutePathBuf;
    #[cfg(windows)]
    use crate::path_safety::CaseSensitivityTestOutcome;
    #[cfg(windows)]
    use crate::path_safety::override_case_sensitivity_for_test;
    use crate::permissions::FileSystemAccessMode;
    use crate::permissions::FileSystemPath;
    use crate::permissions::FileSystemSandboxEntry;
    use crate::permissions::FileSystemSandboxPolicy;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn unreadable_glob_entry(pattern: String) -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern { pattern },
            access: FileSystemAccessMode::Deny,
        }
    }

    #[test]
    fn verbatim_paths_remain_within_their_ordinary_workspace() {
        assert!(path_is_within(
            std::path::Path::new(r"\\?\D:\work\repo\child"),
            std::path::Path::new(r"D:\work\repo")
        ));
        assert!(path_is_within(
            std::path::Path::new(r"\\?\UNC\server\share\repo\child"),
            std::path::Path::new(r"\\server\share\repo")
        ));
    }

    fn unreadable_path_entry(path: PathBuf) -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: AbsolutePathBuf::from_absolute_path(path).expect("absolute path"),
            },
            access: FileSystemAccessMode::Deny,
        }
    }

    #[cfg(unix)]
    fn link_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn link_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        use std::os::windows::process::CommandExt;

        let link_path = link.to_path_buf();
        let link = format!("\"{}\"", link.display());
        let target = format!("\"{}\"", target.display());
        std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link)
            .raw_arg(&target)
            .output()
            .is_ok_and(|output| output.status.success() && link_path.exists())
    }

    #[test]
    fn scan_root_uses_literal_prefix_before_glob() {
        assert_eq!(
            glob_scan_plan("/tmp/work/**/*.env", /*configured_max_depth*/ None).root,
            PathBuf::from("/tmp/work")
        );
        assert_eq!(
            glob_scan_plan(
                r"C:\Users\dev\repo\**\*.env",
                /*configured_max_depth*/ None,
            )
            .root,
            PathBuf::from(r"C:\Users\dev\repo")
        );
        assert_eq!(
            glob_scan_plan(r"C:\*.env", /*configured_max_depth*/ None).root,
            PathBuf::from(r"C:\")
        );
    }

    #[test]
    fn scan_depth_is_bounded_for_non_recursive_globs() {
        assert_eq!(
            glob_scan_plan("/tmp/work/*.env", /*configured_max_depth*/ None).max_depth,
            1
        );
        assert_eq!(
            glob_scan_plan("/tmp/work/*/*.env", /*configured_max_depth*/ None).max_depth,
            2
        );
        assert_eq!(
            glob_scan_plan("/tmp/work/**/*.env", /*configured_max_depth*/ None).max_depth,
            64
        );
    }

    #[test]
    fn configured_depth_caps_recursive_glob_scans() {
        assert_eq!(glob_scan_plan("/tmp/work/**/*.env", Some(2)).max_depth, 2);
        assert_eq!(glob_scan_plan("/tmp/work/*/*.env", Some(1)).max_depth, 1);
    }

    #[test]
    fn recursive_depth_limit_denies_the_unscanned_subtree() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let secret = tmp.path().join("deep").join("secret.env");
        std::fs::create_dir_all(secret.parent().expect("secret parent")).expect("deep directory");
        std::fs::write(&secret, "secret").expect("write secret");
        let mut policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/*.env",
            tmp.path().display()
        ))]);
        policy.glob_scan_max_depth = Some(0);

        assert_eq!(
            resolve_windows_deny_read_paths(&policy, &cwd).expect("resolve"),
            vec![AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute workspace")]
        );
    }

    #[test]
    fn exact_missing_paths_are_preserved() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let missing = tmp.path().join("missing.env");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_path_entry(missing)]);

        assert_eq!(
            resolve_windows_deny_read_paths(&policy, &cwd).expect("resolve"),
            vec![
                AbsolutePathBuf::from_absolute_path(
                    dunce::canonicalize(tmp.path())
                        .expect("canonical tempdir")
                        .join("missing.env")
                )
                .expect("absolute missing")
            ]
        );
    }

    #[test]
    fn glob_patterns_expand_to_existing_matches() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let root_env = tmp.path().join(".env");
        let nested_env = tmp.path().join("app").join(".env");
        let notes = tmp.path().join("app").join("notes.txt");
        std::fs::create_dir_all(notes.parent().expect("parent")).expect("create parent");
        std::fs::write(&root_env, "secret").expect("write root env");
        std::fs::write(&nested_env, "secret").expect("write nested env");
        std::fs::write(&notes, "notes").expect("write notes");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/*.env",
            tmp.path().display()
        ))]);

        let actual: HashSet<PathBuf> = resolve_windows_deny_read_paths(&policy, &cwd)
            .expect("resolve")
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect();
        let expected = [root_env, nested_env].into_iter().collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn invalid_glob_patterns_fail_before_expansion() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/[z-a]",
            tmp.path().display()
        ))]);

        let err = resolve_windows_deny_read_paths(&policy, &cwd).expect_err("invalid glob");
        assert!(
            err.contains("invalid deny-read glob pattern"),
            "unexpected error: {err}"
        );
        assert!(err.contains("invalid range"), "unexpected error: {err}");
        assert!(!err.contains(tmp.path().to_string_lossy().as_ref()));
    }

    #[cfg(windows)]
    #[test]
    fn case_sensitive_scan_directory_is_rejected_before_lowercase_cycle_keys() {
        let tmp = TempDir::new().expect("tempdir");
        let _case_sensitive = override_case_sensitivity_for_test(
            tmp.path(),
            CaseSensitivityTestOutcome::CaseSensitive,
        );
        std::fs::write(tmp.path().join("secret.env"), "secret").expect("write secret");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/*.env",
            tmp.path().display()
        ))]);

        let error = resolve_windows_deny_read_paths(&policy, &cwd)
            .expect_err("case-sensitive scan root must fail closed");

        assert_eq!(error, "deny_read_resolution_case_sensitive_directory");
    }

    #[cfg(windows)]
    #[test]
    fn case_sensitivity_query_failure_keeps_a_distinct_sanitized_category() {
        let tmp = TempDir::new().expect("tempdir");
        let _query_failure = override_case_sensitivity_for_test(
            tmp.path(),
            CaseSensitivityTestOutcome::QueryFailed(5),
        );
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/*.env",
            tmp.path().display()
        ))]);

        let error = resolve_windows_deny_read_paths(&policy, &cwd)
            .expect_err("case-sensitivity query failure must fail closed");

        assert_eq!(error, "deny_read_resolution_case_sensitivity_query_failed");
        assert!(!error.contains(tmp.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn unrelated_path_validation_failure_keeps_a_generic_sanitized_category() {
        let error = anyhow::anyhow!("raw infrastructure detail");

        assert_eq!(
            sanitize_path_safety_error(&error),
            "deny_read_resolution_path_validation_failed"
        );
    }

    #[test]
    fn non_recursive_globs_do_not_expand_nested_matches() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let root_env = tmp.path().join(".env");
        let nested_env = tmp.path().join("app").join(".env");
        std::fs::create_dir_all(nested_env.parent().expect("parent")).expect("create parent");
        std::fs::write(&root_env, "secret").expect("write root env");
        std::fs::write(&nested_env, "secret").expect("write nested env");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/*.env",
            tmp.path().display()
        ))]);

        assert_eq!(
            resolve_windows_deny_read_paths(&policy, &cwd).expect("resolve"),
            vec![AbsolutePathBuf::from_absolute_path(root_env).expect("absolute root env")]
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn aliased_glob_roots_each_preserve_their_lexical_matches() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = AbsolutePathBuf::from_absolute_path(tmp.path()).expect("absolute cwd");
        let target = tmp.path().join("target");
        let alias_a = tmp.path().join("alias-a");
        let alias_b = tmp.path().join("alias-b");
        let secret = target.join("secret.env");
        std::fs::create_dir_all(&target).expect("create target");
        std::fs::write(&secret, "secret").expect("write secret");
        #[cfg(unix)]
        {
            symlink(&target, &alias_a).expect("create alias a");
            symlink(&target, &alias_b).expect("create alias b");
        }
        #[cfg(windows)]
        {
            assert!(
                link_dir(&target, &alias_a),
                "junction fixture must be available"
            );
            assert!(
                link_dir(&target, &alias_b),
                "junction fixture must be available"
            );
        }
        let policy = FileSystemSandboxPolicy::restricted(vec![
            unreadable_glob_entry(format!("{}/**/*.env", alias_a.display())),
            unreadable_glob_entry(format!("{}/**/*.env", alias_b.display())),
        ]);

        let actual: HashSet<PathBuf> = resolve_windows_deny_read_paths(&policy, &cwd)
            .expect("resolve")
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect();
        let expected = [alias_a.join("secret.env"), alias_b.join("secret.env")]
            .into_iter()
            .collect();

        assert_eq!(actual, expected);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn reparse_targets_are_bounded_and_unknown_entries_are_denied() {
        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        std::fs::write(outside.path().join("outside.env"), "secret").expect("outside secret");
        let external_alias = workspace.path().join("external");
        let cycle_alias = workspace.path().join("cycle");
        assert!(
            link_dir(outside.path(), &external_alias),
            "reparse fixture must be available"
        );
        assert!(
            link_dir(workspace.path(), &cycle_alias),
            "cycle reparse fixture must be available"
        );

        let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).expect("absolute cwd");
        let policy = FileSystemSandboxPolicy::restricted(vec![unreadable_glob_entry(format!(
            "{}/**/*.env",
            workspace.path().display()
        ))]);
        let actual: HashSet<PathBuf> = resolve_windows_deny_read_paths(&policy, &cwd)
            .expect("resolve")
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect();

        assert!(actual.contains(&external_alias));
        assert!(!actual.contains(&outside.path().join("outside.env")));
    }
}
