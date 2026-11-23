use std::path::{Path, PathBuf};

pub(super) fn bash_path() -> Result<PathBuf, String> {
    find_bash_on_windows().ok_or_else(|| {
        "Git Bash is required but bash.exe was not found. Install Git for Windows from https://git-scm.com/install/windows, or add the Git bin directory containing bash.exe to PATH."
            .to_string()
    })
}

// System32 下的 bash.exe 是 WSL 启动器。
fn is_system32_bash_launcher(candidate: &Path, system_root: &str) -> bool {
    if !system_root.is_empty() && !starts_with_ignore_ascii_case(candidate, Path::new(system_root)) {
        return false;
    }
    let Some(file_name) = candidate.file_name() else {
        return false;
    };
    let Some(directory) = candidate.parent().and_then(Path::file_name) else {
        return false;
    };
    file_name.eq_ignore_ascii_case("bash.exe") && directory.eq_ignore_ascii_case("System32")
}

fn starts_with_ignore_ascii_case(path: &Path, prefix: &Path) -> bool {
    let mut components = path.components();
    prefix.components().all(|expected| {
        components
            .next()
            .is_some_and(|actual| expected.as_os_str().eq_ignore_ascii_case(actual.as_os_str()))
    })
}

fn find_bash_on_windows() -> Option<PathBuf> {
    static BASH_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if let Some(cached) = BASH_PATH.get() {
        return Some(cached.clone());
    }
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(program_files) = std::env::var_os(var) {
            candidates.push(PathBuf::from(program_files).join("Git\\bin\\bash.exe"));
        }
    }
    let system_root = std::env::var("SystemRoot").unwrap_or_default();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("bash.exe");
            if is_system32_bash_launcher(&candidate, &system_root) {
                continue;
            }
            candidates.push(candidate);
        }
    }
    let found = candidates.into_iter().find(|candidate| candidate.is_file())?;
    let _ = BASH_PATH.set(found.clone());
    Some(found)
}
