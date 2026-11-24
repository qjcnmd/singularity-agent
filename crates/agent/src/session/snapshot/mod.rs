mod restore;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use protocol::{ChangedFile, ChangedFileStatus};

pub use restore::{RestoreBackup, restore};

pub(super) const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "dist-desktop",
    ".venv",
    "__pycache__",
];
const RAW_ATTRIBUTES: &str = "* -text -filter -working-tree-encoding -ident\n";
const MAX_DIFF_BYTES: usize = 200 * 1024;
const MAX_DIFF_LINES: usize = 2_000;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static GIT_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
static SNAPSHOT_LOCKS: OnceLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> = OnceLock::new();

pub fn session_snapshot_directory(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join("snapshots").join(format!("{session_id}.git"))
}

pub fn capture(snapshot_dir: &Path, cwd: &Path) -> io::Result<String> {
    Repo { dir: snapshot_dir, cwd }.with_live_tree(|live, _| Ok(live))
}

pub fn diff_summary(snapshot_dir: &Path, cwd: &Path, base_tree: &str) -> io::Result<Vec<ChangedFile>> {
    let base = validate_tree_hash(base_tree)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(|live, _| {
        let status = repo.diff(base, &live, &["--name-status", "-z"], &[])?;
        let numstat = repo.diff(base, &live, &["--numstat", "-z"], &[])?;
        Ok(parse_diff_summary(&status, &numstat))
    })
}

pub fn file_diff(snapshot_dir: &Path, cwd: &Path, base_tree: &str, rel_path: &str) -> io::Result<String> {
    let base = validate_tree_hash(base_tree)?;
    let rel = validate_relative_path(rel_path)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(|live, _| {
        let flags = &["--no-color", "-U3", "--no-textconv"];
        let mut out = repo.diff(base, &live, flags, &["--", &rel])?;
        #[cfg(windows)]
        if out.is_empty() {
            let names = repo.diff(base, &live, &["--name-only", "-z"], &[])?;
            let target = rel.to_lowercase();
            if let Some(matched) =
                split_nul(&names).map(|p| String::from_utf8_lossy(p)).find(|p| p.to_lowercase() == target)
            {
                out = repo.diff(base, &live, flags, &["--", &matched])?;
            }
        }
        Ok(format_git_diff(&String::from_utf8_lossy(&out)))
    })
}

#[derive(Clone, Copy)]
pub(super) struct Repo<'a> {
    pub(super) dir: &'a Path,
    pub(super) cwd: &'a Path,
}

impl Repo<'_> {
    pub(super) fn with_live_tree<T>(
        self,
        work: impl FnOnce(String, Vec<String>) -> io::Result<T>,
    ) -> io::Result<T> {
        let protected_rels = protected_relative_paths(self.dir, self.cwd)?;
        with_snapshot_lock(self.dir, || {
            self.ensure_initialized()?;
            let protected = self.sync_rules(&protected_rels)?;
            // Rebuild the shadow index from disk so Windows case-only renames are detected cleanly.
            if let Err(err) = fs::remove_file(self.dir.join("index"))
                && err.kind() != ErrorKind::NotFound
            {
                return Err(err);
            }
            let pathspecs = build_add_pathspecs(&protected);
            self.git_with_stdin(
                true,
                false,
                &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
                Some(&pathspecs),
            )?;
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
        collect_nested_git_dirs(self.cwd, self.cwd, protected_rels, &mut protected);
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
        cmd.args(["-c", "core.ignorecase=false"])
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

fn build_add_pathspecs(protected: &[String]) -> Vec<u8> {
    let icase = if cfg!(windows) { "icase," } else { "" };
    let mut out = Vec::from(b".\0" as &[u8]);
    let mut push = |spec: &str| {
        out.extend_from_slice(spec.as_bytes());
        out.push(0);
    };
    push(&format!(":(top,literal,{icase}exclude).git"));
    for rel in protected {
        push(&format!(":(top,literal,{icase}exclude){rel}"));
    }
    for ex in EXCLUDED_DIRS {
        push(&format!(":(top,literal,{icase}exclude){ex}"));
        push(&format!(":(glob,{icase}exclude)**/{ex}/**"));
    }
    out
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
        if dir_key == cwd_key {
            return Err(io::Error::new(ErrorKind::InvalidInput, "不能对应用数据目录自身生成或还原快照。"));
        }
        let is_child = dir_key.strip_prefix(&cwd_key).is_some_and(|rest| rest.starts_with('/'))
            || (cwd_key.ends_with('/') && dir_key.starts_with(&cwd_key));
        if is_child {
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

fn collect_nested_git_dirs(root: &Path, dir: &Path, protected_rels: &[String], out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| EXCLUDED_DIRS.iter().any(|ex| name.eq_ignore_ascii_case(ex)))
        {
            continue;
        }
        let path = entry.path();
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
        if fs::symlink_metadata(path.join(".git")).is_ok() {
            if let Some(joined) = rel
                && !joined.is_empty()
            {
                out.push(joined);
            }
        } else {
            collect_nested_git_dirs(root, &path, protected_rels, out);
        }
    }
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows files carrying FILE_ATTRIBUTE_READONLY require clearing readonly before removal"
)]
pub(super) fn remove_file_clearing_readonly(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            if let Ok(meta) = fs::symlink_metadata(path)
                && !meta.file_type().is_symlink()
                && meta.permissions().readonly()
            {
                let mut perms = meta.permissions();
                perms.set_readonly(false);
                let _ = fs::set_permissions(path, perms);
                return fs::remove_file(path);
            }
            Err(err)
        }
        other => other,
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

fn parse_diff_summary(status_out: &[u8], numstat_out: &[u8]) -> Vec<ChangedFile> {
    let mut files = BTreeMap::new();
    let mut fields = split_nul(status_out);
    while let (Some(status_raw), Some(path_raw)) = (fields.next(), fields.next()) {
        let status = match status_raw.first().copied() {
            Some(b'A') => ChangedFileStatus::Added,
            Some(b'D') => ChangedFileStatus::Deleted,
            _ => ChangedFileStatus::Modified,
        };
        let path = String::from_utf8_lossy(path_raw).into_owned();
        files.insert(path.clone(), ChangedFile { path, added: 0, deleted: 0, status });
    }
    let parse_count = |s: &str| s.parse::<u64>().ok().map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    for entry in split_nul(numstat_out) {
        let text = String::from_utf8_lossy(entry);
        let mut parts = text.splitn(3, '\t');
        if let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
            && let Some(file) = files.get_mut(path)
        {
            file.added = parse_count(added);
            file.deleted = parse_count(deleted);
        }
    }
    files.into_values().collect()
}

fn format_git_diff(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "该文件当前无改动。".to_owned();
    }
    let lines: Vec<&str> = raw.lines().collect();
    let first_hunk = lines.iter().position(|line| line.starts_with("@@ "));
    let header = &lines[..first_hunk.unwrap_or(lines.len())];
    if header
        .iter()
        .any(|l| (l.starts_with("Binary files ") && l.ends_with(" differ")) || *l == "GIT binary patch")
    {
        return "二进制文件已变更，暂不支持文本差异对比。".to_owned();
    }
    let Some(start) = first_hunk else {
        return "无文本内容差异（空文件或文件模式变更）。".to_owned();
    };
    let hunks = &lines[start..];
    let added = u32::try_from(hunks.iter().filter(|l| l.starts_with('+')).count()).unwrap_or(u32::MAX);
    let deleted = u32::try_from(hunks.iter().filter(|l| l.starts_with('-')).count()).unwrap_or(u32::MAX);
    let mut output = format!("+{added} -{deleted}\n");
    for (index, line) in hunks.iter().enumerate() {
        if index >= MAX_DIFF_LINES || output.len() + line.len() + 1 > MAX_DIFF_BYTES {
            output.push_str("[Diff truncated]\n");
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}
