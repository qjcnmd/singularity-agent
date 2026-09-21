//! bash 工具所用 shell 的发现与选择。

use std::path::Path;

/// 用 Git Bash 或 PATH 里的 bash.exe 执行命令。
pub(super) fn shell_command(command: &str) -> Result<(String, Vec<String>), String> {
    Ok((bash_path()?, vec!["-c".to_string(), command.to_string()]))
}

/// 在进程入口处，用与 bash 工具相同的发现规则一次性校验 shell 是否可用。
pub fn ensure_available() -> Result<(), String> {
    bash_path().map(|_| ())
}

/// 可用的 Bash 路径；找不到就给出安装与 PATH 的指引。查找顺序和排除规则见
/// [`find_bash_on_windows`]。
fn bash_path() -> Result<String, String> {
    find_bash_on_windows().ok_or_else(|| {
        "Git Bash is required but bash.exe was not found. Install Git for Windows from https://git-scm.com/install/windows, or add the Git bin directory containing bash.exe to PATH."
            .to_string()
    })
}

/// 判断候选路径是不是 System32 下的 bash 启动器存根。
///
/// System32 下的 bash.exe 是 WSL 的启动器存根：路径语义和进程模型与 Unix shell 完全
/// 不同，在没有安装发行版或服务没运行时还会静默无输出，所以绝不能当作执行后端。
///
/// Windows 路径不区分大小写，而 `Path` 的组件比较是逐字节的，所以这里显式折叠 ASCII
/// 大小写：PATH 里的 `C:\windows\system32` 必须和 `C:\Windows\System32` 一样被排除。
/// `SystemRoot` 缺失时没法做前缀比较，就只按 `System32\bash.exe` 后缀判断。
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

/// 逐组件比较前缀，并按 Windows 的路径语义折叠 ASCII 大小写。
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
    // SystemRoot 只读一次：所有 PATH 候选共用同一个排除基准。
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
            // 保留候选路径的原始写法供实际执行使用，不做任何规范化改写。
            candidates.push(candidate.display().to_string());
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}
