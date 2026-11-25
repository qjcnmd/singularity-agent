mod diff;
mod restore;
mod rollback;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, ErrorKind, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub use diff::{diff_summary, file_diff};
pub use restore::restore;
pub use rollback::RestoreBackup;
use rollback::ensure_no_link_root;

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
        ensure_no_link_root(self.cwd)?;
        let protected_rels = protected_relative_paths(self.dir, self.cwd)?;
        with_snapshot_lock(self.dir, || {
            ensure_no_link_root(self.cwd)?;
            match init {
                InitPolicy::CreateIfMissing => self.ensure_initialized()?,
                InitPolicy::RequireExisting => {
                    if !self.dir.join("HEAD").exists() {
                        return Err(io::Error::new(ErrorKind::NotFound, "工作区快照不存在。"));
                    }
                }
            }
            // Rebuild the shadow index from disk so Windows case-only renames are detected cleanly.
            if let Err(err) = fs::remove_file(self.dir.join("index"))
                && err.kind() != ErrorKind::NotFound
            {
                return Err(err);
            }
            self.sync_info_files()?;
            let mut scan = WorkspaceScan {
                protected: protected_rels.clone(),
                candidates: Vec::new(),
            };
            scan_workspace(self.cwd, self.cwd, &protected_rels, &mut scan);
            scan.protected.sort_unstable();
            scan.protected.dedup();
            let files = self.filter_ignored_paths(&scan.candidates)?;
            if !files.is_empty() {
                self.git_with_stdin(true, true, &["update-index", "--add", "-z", "--stdin"], Some(&files))?;
            }
            let raw = self.git(true, &["write-tree"])?;
            let hash = String::from_utf8(raw).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
            work(validate_tree_hash(hash.trim())?.to_owned(), scan.protected)
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

    fn sync_info_files(self) -> io::Result<()> {
        let info = self.dir.join("info");
        // Clear any stale exclude rules from earlier versions; protected/excluded paths are skipped in Rust.
        write_if_changed(&info.join("exclude"), "")?;
        write_if_changed(&info.join("attributes"), RAW_ATTRIBUTES)
    }

    fn filter_ignored_paths(self, candidates: &[String]) -> io::Result<Vec<u8>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut stdin_bytes = Vec::new();
        for rel in candidates {
            stdin_bytes.extend_from_slice(rel.as_bytes());
            stdin_bytes.push(0);
        }
        let (output, write_res) = self.spawn_git(
            true,
            false,
            &["check-ignore", "--no-index", "-z", "--stdin"],
            Some(&stdin_bytes),
        )?;
        // git check-ignore exits 0 when one or more paths are ignored, and 1 when none are ignored.
        match output.status.code() {
            Some(0) => {
                write_res?;
                let ignored: HashSet<&[u8]> = split_nul(&output.stdout).collect();
                let mut kept = Vec::with_capacity(stdin_bytes.len());
                for rel in candidates {
                    if !ignored.contains(rel.as_bytes()) {
                        kept.extend_from_slice(rel.as_bytes());
                        kept.push(0);
                    }
                }
                Ok(kept)
            }
            Some(1) => {
                write_res?;
                Ok(stdin_bytes)
            }
            _ => Err(io::Error::other(format!(
                "git check-ignore failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
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
        let (output, write_res) = self.spawn_git(with_work_tree, literal_pathspecs, args, input)?;
        if !output.status.success() {
            let sub = args.first().copied().unwrap_or_default();
            return Err(io::Error::other(format!(
                "git {sub} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        write_res?;
        Ok(output.stdout)
    }

    fn spawn_git(
        self,
        with_work_tree: bool,
        literal_pathspecs: bool,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> io::Result<(Output, io::Result<()>)> {
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
        if let Some(bytes) = input {
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
            Ok((wait_res?, write_res))
        } else {
            Ok((cmd.output()?, Ok(())))
        }
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

struct WorkspaceScan {
    protected: Vec<String>,
    candidates: Vec<String>,
}

fn scan_workspace(root: &Path, dir: &Path, protected_rels: &[String], scan: &mut WorkspaceScan) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let is_symlink = ft.is_symlink();
        let is_link = is_symlink
            || (!ft.is_file() && fs::symlink_metadata(&path).is_ok_and(|meta| is_link_metadata(&meta)));
        let Some(joined) = relative_slash_path(root, &path) else {
            continue;
        };
        if is_protected_rel(&joined, protected_rels) {
            continue;
        }
        let is_excluded_name = entry
            .file_name()
            .to_str()
            .is_some_and(|name| EXCLUDED_DIRS.iter().any(|ex| name.eq_ignore_ascii_case(ex)));
        if is_link {
            scan.protected.push(joined);
            continue;
        }
        if ft.is_file() {
            if !(dir == root && is_excluded_name) {
                scan.candidates.push(joined);
            }
            continue;
        }
        if !ft.is_dir() || is_excluded_name {
            continue;
        }
        if fs::symlink_metadata(path.join(".git")).is_ok() {
            scan.protected.push(joined);
        } else {
            scan_workspace(root, &path, protected_rels, scan);
        }
    }
}

fn relative_slash_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path
        .strip_prefix(root)
        .ok()?
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!rel.is_empty()).then_some(rel)
}

pub(super) fn is_link_metadata(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
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
