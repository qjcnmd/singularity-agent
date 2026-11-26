mod parse;

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

use protocol::{ChangedFile, ChangedFileStatus};

use self::parse::{
    DiffSection, commit_section, count_lines_in_bytes, format_git_diff, format_untracked_bytes,
    parse_raw_numstat,
};

const MAX_BATCH_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_UNTRACKED_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 4 * 1024;
const EMPTY_TREE_SHA1: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const EMPTY_TREE_SHA256: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

const DIFF_COMMON_ARGS: &[&str] = &[
    "--literal-pathspecs",
    "-c",
    "core.quotepath=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.relative=false",
    "diff",
];
const DIFF_PATCH_ARGS: &[&str] = &["--no-color", "-U3", "--no-textconv", "--no-ext-diff"];
const DIFF_SUMMARY_ARGS: &[&str] = &["--raw", "--numstat", "-z"];
const LS_UNTRACKED_ARGS: &[&str] = &[
    "--literal-pathspecs",
    "-c",
    "core.quotepath=false",
    "ls-files",
    "--others",
    "--exclude-standard",
    "-z",
];

static GIT_BINARY: OnceLock<PathBuf> = OnceLock::new();

enum DiffAttempt<T> {
    Ok(T),
    NotGitRepo,
    UnbornHead,
}

pub(crate) fn changed_files(cwd: &Path, include_diffs: bool) -> io::Result<Vec<ChangedFile>> {
    let Some((git, prefix)) = repo_prefix(cwd) else {
        return Ok(Vec::new());
    };
    let mut files = BTreeMap::<String, ChangedFile>::new();
    let mut remaining_budget = if include_diffs { MAX_BATCH_DIFF_BYTES } else { 0 };

    if include_diffs {
        apply_batch_diffs(git, cwd, &prefix, &mut files, &mut remaining_budget)?;
    } else if let Some(raw_numstat) =
        run_git_diff_with_empty_tree_fallback(git, cwd, DIFF_SUMMARY_ARGS, &["--", "."])?
    {
        parse_raw_numstat(&raw_numstat, &prefix, &mut files);
    }

    let ls_args: Vec<&str> = LS_UNTRACKED_ARGS.iter().copied().chain(["--", "."]).collect();
    let untracked_out = run_git_ok(git, cwd, &ls_args)?;
    for raw in untracked_out.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = String::from_utf8_lossy(raw).replace('\\', "/");
        if rel.is_empty() || contains_path(&files, &rel) {
            continue;
        }
        let Ok(Some((bytes, truncated_file))) = read_regular_bounded(&cwd.join(&rel), MAX_UNTRACKED_BYTES)
        else {
            continue;
        };
        let added = count_lines_in_bytes(&bytes);
        let diff = if remaining_budget > 0 {
            let formatted = format_untracked_bytes(&bytes, truncated_file);
            if formatted.len() <= remaining_budget {
                remaining_budget -= formatted.len();
                Some(formatted)
            } else {
                None
            }
        } else {
            None
        };
        files.insert(
            rel.clone(),
            ChangedFile {
                path: rel,
                added,
                deleted: 0,
                status: ChangedFileStatus::Added,
                diff,
            },
        );
    }
    Ok(files.into_values().collect())
}

pub(crate) fn file_diff(cwd: &Path, rel_path: &str) -> io::Result<String> {
    let rel = validate_relative_path(rel_path)?;
    let Some(git) = git_binary() else {
        return Ok("当前目录不是 Git 仓库。".to_owned());
    };
    match single_file_diff(git, cwd, &rel)? {
        SingleDiff::NotGitRepo => return Ok("当前目录不是 Git 仓库。".to_owned()),
        SingleDiff::Output(text) => return Ok(text),
        SingleDiff::Empty => {}
    }
    #[cfg(windows)]
    if let Ok(listed) = changed_files(cwd, false) {
        let target = rel.to_lowercase();
        if let Some(matched) = listed.into_iter().find(|f| f.path.to_lowercase() == target)
            && matched.path != rel
            && let SingleDiff::Output(text) = single_file_diff(git, cwd, &matched.path)?
        {
            return Ok(text);
        }
    }
    Ok("该文件当前无改动。".to_owned())
}

enum SingleDiff {
    NotGitRepo,
    Empty,
    Output(String),
}

fn single_file_diff(git: &Path, cwd: &Path, rel: &str) -> io::Result<SingleDiff> {
    let Some(out) = run_git_diff_with_empty_tree_fallback(git, cwd, DIFF_PATCH_ARGS, &["--", rel])? else {
        return Ok(SingleDiff::NotGitRepo);
    };
    if !out.is_empty() {
        return Ok(SingleDiff::Output(format_git_diff(&String::from_utf8_lossy(&out))));
    }
    let ls_args: Vec<&str> = LS_UNTRACKED_ARGS.iter().copied().chain(["--", rel]).collect();
    let untracked = run_git_ok(git, cwd, &ls_args)?;
    if !untracked.is_empty() {
        return match read_regular_bounded(&cwd.join(rel), MAX_UNTRACKED_BYTES)? {
            Some((bytes, truncated)) => Ok(SingleDiff::Output(format_untracked_bytes(&bytes, truncated))),
            None => Ok(SingleDiff::Empty),
        };
    }
    Ok(SingleDiff::Empty)
}

fn contains_path(files: &BTreeMap<String, ChangedFile>, rel: &str) -> bool {
    if files.contains_key(rel) {
        return true;
    }
    #[cfg(windows)]
    {
        let target = rel.to_lowercase();
        files.keys().any(|key| key.to_lowercase() == target)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn repo_prefix(cwd: &Path) -> Option<(&'static Path, String)> {
    let git = git_binary()?;
    let out = run_git(git, cwd, &["rev-parse", "--is-inside-work-tree", "--show-prefix"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    if lines.next()?.trim() != "true" {
        return None;
    }
    let prefix = lines.next().unwrap_or("").trim_end_matches('\r').replace('\\', "/");
    Some((git, prefix))
}

fn empty_tree_oid(git: &Path, cwd: &Path) -> &'static str {
    if let Ok(out) = run_git(git, cwd, &["rev-parse", "--show-object-format"])
        && out.status.success()
        && String::from_utf8_lossy(&out.stdout).trim().eq_ignore_ascii_case("sha256")
    {
        return EMPTY_TREE_SHA256;
    }
    EMPTY_TREE_SHA1
}

fn git_binary() -> Option<&'static Path> {
    if let Some(path) = GIT_BINARY.get() {
        return Some(path.as_path());
    }
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
            [
                base.join("Git\\mingw64\\bin\\git.exe"),
                base.join("Git\\cmd\\git.exe"),
                base.join("Git\\bin\\git.exe"),
            ]
        });
    let found = path_hits.chain(fallbacks).find(|p| p.is_file())?;
    let resolved = resolve_direct_git(found);
    Some(GIT_BINARY.get_or_init(|| resolved).as_path())
}

fn resolve_direct_git(found: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let parent_is_cmd = found
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd"));
        if parent_is_cmd && let Some(root) = found.parent().and_then(Path::parent) {
            for rel in ["mingw64\\bin\\git.exe", "mingw32\\bin\\git.exe"] {
                let direct = root.join(rel);
                if direct.is_file() {
                    return direct;
                }
            }
        }
    }
    found
}

fn git_command(git: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(git);
    cmd.current_dir(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_MESSAGES", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn run_git(git: &Path, cwd: &Path, args: &[&str]) -> io::Result<Output> {
    git_command(git, cwd, args).output()
}

fn run_git_ok(git: &Path, cwd: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = run_git(git, cwd, args)?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::other(format!("git {:?} 失败：{}", args.first().unwrap_or(&""), stderr.trim())))
}

fn classify_diff_stderr(stderr: &str) -> Option<DiffAttempt<()>> {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not a git repository") || lower.contains("must be run in a work tree") {
        Some(DiffAttempt::NotGitRepo)
    } else if lower.contains("bad revision 'head'") || lower.contains("ambiguous argument 'head'") {
        Some(DiffAttempt::UnbornHead)
    } else {
        None
    }
}

fn run_git_diff_with_empty_tree_fallback(
    git: &Path,
    cwd: &Path,
    mode_args: &[&str],
    tail_args: &[&str],
) -> io::Result<Option<Vec<u8>>> {
    let run_with_base = |base: &str| -> io::Result<DiffAttempt<Vec<u8>>> {
        let args: Vec<&str> = DIFF_COMMON_ARGS
            .iter()
            .copied()
            .chain(mode_args.iter().copied())
            .chain([base])
            .chain(tail_args.iter().copied())
            .collect();
        let out = run_git(git, cwd, &args)?;
        if out.status.success() {
            return Ok(DiffAttempt::Ok(out.stdout));
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        match classify_diff_stderr(&stderr) {
            Some(DiffAttempt::NotGitRepo) => Ok(DiffAttempt::NotGitRepo),
            Some(DiffAttempt::UnbornHead) => Ok(DiffAttempt::UnbornHead),
            _ => Err(io::Error::other(format!("git diff 失败：{}", stderr.trim()))),
        }
    };
    match run_with_base("HEAD")? {
        DiffAttempt::Ok(out) => Ok(Some(out)),
        DiffAttempt::NotGitRepo => Ok(None),
        DiffAttempt::UnbornHead => match run_with_base(empty_tree_oid(git, cwd))? {
            DiffAttempt::Ok(out) => Ok(Some(out)),
            DiffAttempt::NotGitRepo => Ok(None),
            DiffAttempt::UnbornHead => Ok(Some(Vec::new())),
        },
    }
}

fn apply_batch_diffs(
    git: &Path,
    cwd: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, ChangedFile>,
    remaining_budget: &mut usize,
) -> io::Result<()> {
    match stream_batch_diffs(git, cwd, "HEAD", prefix, files, remaining_budget)? {
        DiffAttempt::Ok(()) | DiffAttempt::NotGitRepo => Ok(()),
        DiffAttempt::UnbornHead => {
            let empty = empty_tree_oid(git, cwd);
            let _ = stream_batch_diffs(git, cwd, empty, prefix, files, remaining_budget)?;
            Ok(())
        }
    }
}

fn stream_batch_diffs(
    git: &Path,
    cwd: &Path,
    base: &str,
    prefix: &str,
    files: &mut BTreeMap<String, ChangedFile>,
    remaining_budget: &mut usize,
) -> io::Result<DiffAttempt<()>> {
    let args: Vec<&str> = DIFF_COMMON_ARGS
        .iter()
        .copied()
        .chain(DIFF_PATCH_ARGS.iter().copied())
        .chain([base, "--", "."])
        .collect();
    let mut child = git_command(git, cwd, &args).spawn()?;
    let stdout = child.stdout.take().expect("git diff stdout piped");
    let stderr = child.stderr.take().expect("git diff stderr piped");

    let (read_res, stderr_text) = std::thread::scope(|scope| {
        let stderr_handle = scope.spawn(move || {
            let mut reader = stderr;
            let mut buf = Vec::new();
            let _ = reader.by_ref().take(MAX_STDERR_BYTES).read_to_end(&mut buf);
            let _ = io::copy(&mut reader, &mut io::sink());
            String::from_utf8_lossy(&buf).into_owned()
        });

        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let mut raw_line = Vec::new();
        let mut current: Option<DiffSection> = None;
        let read_res: io::Result<()> = loop {
            raw_line.clear();
            match reader.read_until(b'\n', &mut raw_line) {
                Ok(0) => {
                    if let Some(section) = current.take() {
                        commit_section(section, prefix, files, remaining_budget);
                    }
                    break Ok(());
                }
                Ok(_) => {
                    while matches!(raw_line.last(), Some(b'\n' | b'\r')) {
                        raw_line.pop();
                    }
                    let line = String::from_utf8_lossy(&raw_line);
                    if line.starts_with("diff --git ") {
                        if let Some(section) = current.take() {
                            commit_section(section, prefix, files, remaining_budget);
                        }
                        current = Some(DiffSection::new(line.into_owned()));
                    } else if let Some(section) = current.as_mut() {
                        section.push_line(&line, *remaining_budget > 0);
                    }
                }
                Err(error) => break Err(error),
            }
        };
        let stderr_text = stderr_handle.join().unwrap_or_default();
        (read_res, stderr_text)
    });

    let status = child.wait()?;
    read_res?;
    if status.success() {
        return Ok(DiffAttempt::Ok(()));
    }
    match classify_diff_stderr(&stderr_text) {
        Some(DiffAttempt::NotGitRepo) => Ok(DiffAttempt::NotGitRepo),
        Some(DiffAttempt::UnbornHead) => Ok(DiffAttempt::UnbornHead),
        _ => Err(io::Error::other(format!("git diff 失败：{}", stderr_text.trim()))),
    }
}

fn read_regular_bounded(path: &Path, limit: usize) -> io::Result<Option<(Vec<u8>, bool)>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref().take((limit + 1) as u64).read_to_end(&mut buf)?;
    let truncated = buf.len() > limit;
    if truncated {
        buf.truncate(limit);
    }
    Ok(Some((buf, truncated)))
}

fn validate_relative_path(rel_path: &str) -> io::Result<String> {
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
