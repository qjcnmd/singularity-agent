//! bash 工具的后端 shell 发现与选择。

use std::path::Path;

/// 使用 Git Bash 或 PATH 中的 bash.exe 执行命令。
pub(super) fn shell_command(command: &str) -> Result<(String, Vec<String>), String> {
    Ok((bash_path()?, vec!["-c".to_string(), command.to_string()]))
}

/// 在进程入口点用与 bash 工具相同的发现规则一次性校验 shell 前置。
pub fn ensure_available() -> Result<(), String> {
    bash_path().map(|_| ())
}

/// 可用的 Bash 路径，否则给出安装与 PATH 指引。查找顺序与排除规则见
/// [`find_bash_on_windows`]。
fn bash_path() -> Result<String, String> {
    find_bash_on_windows().ok_or_else(|| {
        "Git Bash is required but bash.exe was not found. Install Git for Windows from https://git-scm.com/install/windows, or add the Git bin directory containing bash.exe to PATH."
            .to_string()
    })
}

/// 候选路径是否是 System32 下的 bash 启动器存根。
///
/// System32 下的 bash.exe 是 WSL 启动器存根：路径语义、进程模型与 Unix shell
/// 完全不同，且在无发行版/服务未运行的环境中静默无输出，绝不能作为 bash 工具
/// 的执行后端。
///
/// Windows 路径不区分大小写，而 `Path` 的组件比较是逐字节的，因此这里显式折叠
/// ASCII 大小写：PATH 里的 `C:\windows\system32` 与 `C:\Windows\System32` 是同一
/// 个目录，必须同样被排除。`SystemRoot` 由调用方读取一次后传入；它缺失时无法做
/// 前缀比较，只按 `System32\bash.exe` 后缀判定。
fn is_system32_bash_launcher(candidate: &Path, system_root: &str) -> bool {
    if !system_root.is_empty() && !starts_with_ignore_ascii_case(candidate, Path::new(system_root))
    {
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

/// 组件级前缀比较，按 Windows 的路径语义折叠 ASCII 大小写。
fn starts_with_ignore_ascii_case(path: &Path, prefix: &Path) -> bool {
    let mut components = path.components();
    prefix.components().all(|expected| {
        components.next().is_some_and(|actual| {
            expected
                .as_os_str()
                .eq_ignore_ascii_case(actual.as_os_str())
        })
    })
}

fn find_bash_on_windows() -> Option<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(program_files) = std::env::var(var) {
            candidates.push(format!("{program_files}\\Git\\bin\\bash.exe"));
        }
    }
    // SystemRoot 只读取一次：全部 PATH 候选共用同一个排除基准。
    let system_root = std::env::var("SystemRoot").unwrap_or_default();
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("bash.exe");
            if is_system32_bash_launcher(&candidate, &system_root) {
                continue;
            }
            // 保留原始候选路径用于实际执行，不做任何规范化改写。
            candidates.push(candidate.display().to_string());
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;

    use super::is_system32_bash_launcher;

    /// System32 下的启动器存根在任何大小写形式下都被排除，Git 安装位置与其它
    /// 目录下的 bash.exe 不受影响。
    #[test]
    fn the_system32_launcher_is_excluded_regardless_of_path_case() {
        let system_root = "C:\\Windows";
        for candidate in [
            "C:\\Windows\\System32\\bash.exe",
            "c:\\windows\\system32\\bash.exe",
            "C:\\WINDOWS\\SYSTEM32\\BASH.EXE",
        ] {
            assert!(
                is_system32_bash_launcher(Path::new(candidate), system_root),
                "{candidate} is the System32 launcher stub"
            );
        }
        for candidate in [
            "C:\\Program Files\\Git\\bin\\bash.exe",
            "C:\\Program Files\\Git\\usr\\bin\\bash.exe",
            "C:\\Windows\\SysWOW64\\bash.exe",
            "C:\\Windows\\System32\\not-bash.exe",
            "D:\\Windows\\System32\\bash.exe",
            "C:\\tools\\System32\\bash.exe",
        ] {
            assert!(
                !is_system32_bash_launcher(Path::new(candidate), system_root),
                "{candidate} must stay a candidate"
            );
        }
        // 缺少 SystemRoot 时仍按 System32\bash.exe 后缀排除；后缀本身不匹配的
        // 路径在任何情况下都不会被当成存根。
        assert!(is_system32_bash_launcher(
            Path::new("C:\\Windows\\System32\\bash.exe"),
            ""
        ));
        assert!(!is_system32_bash_launcher(Path::new("bash.exe"), ""));
    }
}
