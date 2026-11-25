mod diff;
mod restore;
mod rollback;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub use diff::{diff_summary, file_diff};
pub use restore::restore;
pub use rollback::RestoreBackup;

pub(super) const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "desktop-dist",
    ".venv",
    "__pycache__",
];
const RAW_ATTRIBUTES: &str = "* -text -filter -working-tree-encoding -ident\n";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(any(windows, test))]
const REPARSE_CLASSIFY_ARG_BUDGET: usize = 12_000;

static GIT_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
static SNAPSHOT_LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

pub fn session_snapshot_directory(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join("snapshots").join(format!("{session_id}.git"))
}

pub fn remove(snapshot_dir: &Path) -> io::Result<()> {
    with_snapshot_lock(snapshot_dir, || rollback::remove_dir_clearing_readonly(snapshot_dir))
}

pub fn capture(snapshot_dir: &Path, cwd: &Path) -> io::Result<String> {
    Repo { dir: snapshot_dir, cwd }.with_live_tree(InitPolicy::CreateIfMissing, |live, _| Ok(live))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InitPolicy {
    CreateIfMissing,
    RequireExisting,
}

#[derive(Clone, Copy)]
pub(super) struct Repo<'a> {
    pub(super) dir: &'a Path,
    pub(super) cwd: &'a Path,
}

impl Repo<'_> {
    pub(super) fn with_live_tree<T>(
        self,
        init: InitPolicy,
        work: impl FnOnce(String, Vec<String>) -> io::Result<T>,
    ) -> io::Result<T> {
        let protected_rels = protected_relative_paths(self.dir, self.cwd)?;
        with_snapshot_lock(self.dir, || {
            match init {
                InitPolicy::CreateIfMissing => self.ensure_initialized()?,
                InitPolicy::RequireExisting => {
                    if !self.dir.join("HEAD").exists() {
                        return Err(io::Error::new(ErrorKind::NotFound, "工作区快照不存在。"));
                    }
                }
            }
            // Rebuild the shadow index from disk so Windows case-only renames are detected cleanly
            // and reparse classification runs against an empty index.
            if let Err(err) = fs::remove_file(self.dir.join("index"))
                && err.kind() != ErrorKind::NotFound
            {
                return Err(err);
            }
            let protected = self.sync_rules(&protected_rels)?;
            let pathspecs = build_static_exclude_pathspecs();
            let mut ls_args = Vec::with_capacity(5 + pathspecs.len());
            ls_args.extend(["ls-files", "--others", "--exclude-standard", "-z", "--"]);
            ls_args.extend(pathspecs.iter().map(String::as_str));
            let raw_files = self.git_with_stdin(true, false, &ls_args, None)?;
            let files = filter_protected_nul_paths(&raw_files, &protected);
            if !files.is_empty() {
                self.git_with_stdin(true, true, &["update-index", "--add", "-z", "--stdin"], Some(&files))?;
            }
            let raw = self.git(true, &["write-tree"])?;
            let hash = String::from_utf8(raw).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
            work(validate_tree_hash(hash.trim())?.to_owned(), protected)
        })
    }

    fn ensure_initialized(self) -> io::Result<()> {
        if self.dir.join("HEAD").exists() {
            return Ok(());
        }
        fs::create_dir_all(self.dir)?;
        self.git(false, &["init", "--bare"])?;
        for (k, v) in [
            ("core.autocrlf", "false"),
            ("core.ignorecase", "false"),
            ("core.quotepath", "false"),
            ("core.longpaths", "true"),
        ] {
            self.git(false, &["config", k, v])?;
        }
        Ok(())
    }

    fn sync_rules(self, protected_rels: &[String]) -> io::Result<Vec<String>> {
        let mut protected = protected_rels.to_vec();
        let mut reparse_candidates = Vec::new();
        scan_protected_paths(self.cwd, self.cwd, protected_rels, &mut protected, &mut reparse_candidates);
        #[cfg(windows)]
        if !reparse_candidates.is_empty() {
            protected.extend(self.classify_junction_candidates(&reparse_candidates)?);
        }
        protected.sort_unstable();
        protected.dedup();
        let mut exclude_content = String::new();
        for dir in EXCLUDED_DIRS {
            exclude_content.push_str(dir);
            exclude_content.push_str("/\n");
        }
        for rel in &protected {
            exclude_content.push('/');
            for ch in rel.chars() {
                if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
                    exclude_content.push('\\');
                }
                exclude_content.push(ch);
            }
            exclude_content.push_str("/\n");
        }
        let info = self.dir.join("info");
        write_if_changed(&info.join("exclude"), &exclude_content)?;
        write_if_changed(&info.join("attributes"), RAW_ATTRIBUTES)?;
        Ok(protected)
    }

    #[cfg(windows)]
    fn classify_junction_candidates(self, candidates: &[String]) -> io::Result<Vec<String>> {
        // Git for Windows treats directory symlinks (mklink /D) as 120000 symlinks (no trailing '/')
        // and NTFS junctions (mklink /J) as directories (trailing '/'). Query Git in small batches
        // so directory symlinks remain snapshotted while junctions stay excluded.
        let mut git_symlinks = HashSet::new();
        for chunk in batch_by_arg_budget(candidates, REPARSE_CLASSIFY_ARG_BUDGET) {
            let mut args = Vec::with_capacity(5 + chunk.len());
            args.extend(["ls-files", "--others", "--directory", "-z", "--"]);
            args.extend(chunk.iter().map(String::as_str));
            let out = self.git(true, &args)?;
            for entry in split_nul(&out) {
                let text = String::from_utf8_lossy(entry);
                if !text.ends_with('/') {
                    git_symlinks.insert(path_key(&text));
                }
            }
        }
        Ok(candidates.iter().filter(|rel| !git_symlinks.contains(&path_key(rel))).cloned().collect())
    }

    pub(super) fn diff(self, base: &str, live: &str, flags: &[&str], tail: &[&str]) -> io::Result<Vec<u8>> {
        let mut args = Vec::with_capacity(5 + flags.len() + tail.len());
        args.extend(["diff", "--no-renames", "--no-ext-diff"]);
        args.extend(flags);
        args.extend([base, live]);
        args.extend(tail);
        self.git(true, &args)
    }

    pub(super) fn git(self, with_work_tree: bool, args: &[&str]) -> io::Result<Vec<u8>> {
        self.git_with_stdin(with_work_tree, true, args, None)
    }

    pub(super) fn git_with_stdin(
        self,
        with_work_tree: bool,
        literal_pathspecs: bool,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> io::Result<Vec<u8>> {
        let mut cmd = Command::new(git_binary()?);
        // Force core.symlinks=true so checkout-index never silently degrades 120000 symlinks
        // into plain text files on Windows bare repos where git init defaults core.symlinks=false.
        cmd.args(["-c", "core.ignorecase=false", "-c", "core.symlinks=true"])
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Strip Windows verbatim `\\?\` prefix because Git for Windows rejects it in env vars,
            // and isolate system/global gitconfig so external filters (e.g. LFS) cannot interfere.
            .env("GIT_DIR", app_core::display_path(self.dir))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_TERMINAL_PROMPT", "0");
        if literal_pathspecs {
            cmd.env("GIT_LITERAL_PATHSPECS", "1");
        }
        if with_work_tree {
            let work_tree = app_core::display_path(self.cwd);
            cmd.env("GIT_INDEX_FILE", app_core::display_path(&self.dir.join("index")))
                .env("GIT_WORK_TREE", &work_tree)
                .current_dir(&work_tree);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = if let Some(bytes) = input {
            let mut child = cmd.spawn()?;
            let mut pipe = child.stdin.take();
            let (write_res, wait_res) = std::thread::scope(|scope| {
                let writer = scope.spawn(move || {
                    let Some(stdin) = pipe.take() else {
                        return Ok(());
                    };
                    let mut stdin = stdin;
                    let res = stdin.write_all(bytes);
                    drop(stdin);
                    res
                });
                let wait_res = child.wait_with_output();
                let write_res = writer.join().expect("git stdin writer thread panicked");
                (write_res, wait_res)
            });
            let output = wait_res?;
            if output.status.success() {
                write_res?;
            }
            output
        } else {
            cmd.output()?
        };
        if !output.status.success() {
            let sub = args.first().copied().unwrap_or_default();
            return Err(io::Error::other(format!(
                "git {sub} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

pub(super) fn with_snapshot_lock<T>(
    snapshot_dir: &Path,
    work: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let locks = SNAPSHOT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = app_core::workspace_key(snapshot_dir);
    let lock = {
        let mut map = locks.lock().expect("snapshot lock table poisoned");
        let lock = map.get(&key).and_then(Weak::upgrade).unwrap_or_default();
        map.insert(key, Arc::downgrade(&lock));
        lock
    };
    let result = {
        let _guard = lock.lock().expect("snapshot mutex poisoned");
        work()
    };
    drop(lock);
    locks.lock().expect("snapshot lock table poisoned").retain(|_, e| e.strong_count() > 0);
    result
}

fn build_static_exclude_pathspecs() -> Vec<String> {
    let icase = if cfg!(windows) { "icase," } else { "" };
    let mut out = Vec::with_capacity(1 + EXCLUDED_DIRS.len() * 2);
    out.push(".".to_owned());
    for ex in EXCLUDED_DIRS {
        out.push(format!(":(top,literal,{icase}exclude){ex}"));
        out.push(format!(":(glob,{icase}exclude)**/{ex}/**"));
    }
    out
}

fn filter_protected_nul_paths(raw: &[u8], protected: &[String]) -> Vec<u8> {
    if protected.is_empty() || raw.is_empty() {
        return raw.to_vec();
    }
    let protected_set: HashSet<String> = protected.iter().map(|p| path_key(p)).collect();
    let mut out = Vec::with_capacity(raw.len());
    for entry in split_nul(raw) {
        let rel = String::from_utf8_lossy(entry);
        if !has_protected_prefix(&rel, &protected_set) {
            out.extend_from_slice(entry);
            out.push(0);
        }
    }
    out
}

fn has_protected_prefix(rel: &str, protected_set: &HashSet<String>) -> bool {
    let key = path_key(rel);
    if protected_set.contains(&key) {
        return true;
    }
    for (idx, byte) in key.bytes().enumerate() {
        if byte == b'/' && protected_set.contains(&key[..idx]) {
            return true;
        }
    }
    false
}

fn write_if_changed(path: &Path, content: &str) -> io::Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

pub(super) fn canonicalize_existing(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut full = canon;
            for part in tail.into_iter().rev() {
                full.push(part);
            }
            return full;
        }
        let Some(name) = cur.file_name() else {
            return path.components().collect();
        };
        tail.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            return path.components().collect();
        };
        cur = parent;
    }
}

fn protected_relative_paths(snapshot_dir: &Path, cwd: &Path) -> io::Result<Vec<String>> {
    let cwd_canon = canonicalize_existing(cwd);
    let cwd_key = app_core::workspace_key(&cwd_canon);
    let cwd_count = cwd_canon.components().count();
    let mut dirs = vec![snapshot_dir.to_path_buf()];
    if let Some(snapshots) = snapshot_dir.parent()
        && snapshots.file_name().is_some_and(|n| n == "snapshots")
        && let Some(sessions) = snapshots.parent()
        && sessions.file_name().is_some_and(|n| n == "sessions")
        && let Some(home) = sessions.parent()
    {
        dirs.push(home.to_path_buf());
    }
    let mut rels = Vec::new();
    for dir in dirs {
        let dir_canon = canonicalize_existing(&dir);
        let dir_key = app_core::workspace_key(&dir_canon);
        if app_core::workspace_key_contains(&dir_key, &cwd_key) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "不能对应用数据目录或其子目录生成或还原快照。",
            ));
        }
        if app_core::workspace_key_contains(&cwd_key, &dir_key) {
            let rel = dir_canon
                .components()
                .skip(cwd_count)
                .filter_map(|c| match c {
                    Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            if !rel.is_empty() {
                rels.push(rel);
            }
        }
    }
    rels.sort_unstable();
    rels.dedup();
    Ok(rels)
}

pub(super) fn is_protected_rel(rel: &str, protected_rels: &[String]) -> bool {
    let matches_prefix = |prefix: &str| -> bool {
        if cfg!(windows) {
            rel.eq_ignore_ascii_case(prefix)
                || (rel.len() > prefix.len()
                    && rel.as_bytes()[prefix.len()] == b'/'
                    && rel[..prefix.len()].eq_ignore_ascii_case(prefix))
        } else {
            rel == prefix || rel.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
        }
    };
    matches_prefix(".git") || protected_rels.iter().any(|p| matches_prefix(p))
}

fn scan_protected_paths(
    root: &Path,
    dir: &Path,
    protected_rels: &[String],
    out: &mut Vec<String>,
    reparse_candidates: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_reparse = fs::symlink_metadata(&path).is_ok_and(|meta| is_dir_reparse_point(&meta));
        if !is_reparse && (!ft.is_dir() || ft.is_symlink()) {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| EXCLUDED_DIRS.iter().any(|ex| name.eq_ignore_ascii_case(ex)))
        {
            continue;
        }
        let rel = path.strip_prefix(root).ok().map(|r| {
            r.components()
                .filter_map(|c| match c {
                    Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/")
        });
        if let Some(joined) = rel.as_deref()
            && is_protected_rel(joined, protected_rels)
        {
            continue;
        }
        if is_reparse {
            if let Some(joined) = rel
                && !joined.is_empty()
            {
                reparse_candidates.push(joined);
            }
        } else if fs::symlink_metadata(path.join(".git")).is_ok() {
            if let Some(joined) = rel
                && !joined.is_empty()
            {
                out.push(joined);
            }
        } else {
            scan_protected_paths(root, &path, protected_rels, out, reparse_candidates);
        }
    }
}

pub(super) fn is_dir_reparse_point(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        let attrs = meta.file_attributes();
        (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0 && (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

pub(super) fn path_key(rel: &str) -> String {
    if cfg!(windows) {
        rel.to_lowercase()
    } else {
        rel.to_owned()
    }
}

fn git_binary() -> io::Result<&'static Path> {
    let found = GIT_BINARY.get_or_init(|| {
        let exe = if cfg!(windows) { "git.exe" } else { "git" };
        let path_hits = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(exe));
        let fallbacks = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
            .into_iter()
            .filter_map(std::env::var_os)
            .flat_map(|root| {
                let base = PathBuf::from(root);
                [base.join("Git\\cmd\\git.exe"), base.join("Git\\bin\\git.exe")]
            });
        path_hits.chain(fallbacks).find(|p| p.is_file())
    });
    found
        .as_deref()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "未找到 git.exe，无法记录或恢复工作区快照。"))
}

pub(super) fn validate_tree_hash(tree: &str) -> io::Result<&str> {
    if matches!(tree.len(), 40 | 64) && tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(tree)
    } else {
        Err(io::Error::new(ErrorKind::InvalidInput, format!("invalid snapshot tree hash: {tree}")))
    }
}

pub(super) fn validate_relative_path(rel_path: &str) -> io::Result<String> {
    let normalized = rel_path.replace('\\', "/");
    let parts = Path::new(&normalized)
        .components()
        .map(|c| match c {
            Component::Normal(p) => Ok(p.to_string_lossy().into_owned()),
            _ => Err(io::Error::new(ErrorKind::InvalidInput, format!("invalid relative path: {rel_path}"))),
        })
        .collect::<io::Result<Vec<_>>>()?;
    if parts.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "relative path is empty"));
    }
    Ok(parts.join("/"))
}

pub(super) fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.strip_suffix(b"\0").unwrap_or(bytes).split(|b| *b == 0).filter(|s| !s.is_empty())
}

#[cfg(any(windows, test))]
fn batch_by_arg_budget(items: &[String], budget: usize) -> Vec<&[String]> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    while start < items.len() {
        let mut end = start;
        let mut used = 0usize;
        while end < items.len() {
            let cost = items[end].encode_utf16().count() + 4;
            if end > start && used + cost > budget {
                break;
            }
            used += cost;
            end += 1;
        }
        batches.push(&items[start..end]);
        start = end;
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::{REPARSE_CLASSIFY_ARG_BUDGET, batch_by_arg_budget};

    #[test]
    fn batch_by_arg_budget_splits_long_paths_under_windows_argv_limit() {
        let long_paths: Vec<String> = (0..100).map(|i| format!("{}/{i:04}", "a".repeat(550))).collect();
        let batches = batch_by_arg_budget(&long_paths, REPARSE_CLASSIFY_ARG_BUDGET);
        assert!(batches.len() >= 5, "expected multiple batches, got {}", batches.len());
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, long_paths.len());
        for batch in batches {
            let cost: usize = batch.iter().map(|p| p.encode_utf16().count() + 4).sum();
            assert!(cost <= REPARSE_CLASSIFY_ARG_BUDGET || batch.len() == 1);
        }
    }
}
