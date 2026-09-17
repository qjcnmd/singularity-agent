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

fn find_bash_on_windows() -> Option<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(program_files) = std::env::var(var) {
            candidates.push(format!("{program_files}\\Git\\bin\\bash.exe"));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("bash.exe");
            // System32 下的 bash.exe 是 WSL 启动器存根：路径语义、进程模型与
            // Unix shell 完全不同，且在无发行版/服务未运行的环境中静默无输出，
            // 绝不能作为 bash 工具的执行后端。
            if candidate.starts_with(std::env::var("SystemRoot").unwrap_or_default())
                && candidate.ends_with("System32\\bash.exe")
            {
                continue;
            }
            candidates.push(candidate.display().to_string());
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}
