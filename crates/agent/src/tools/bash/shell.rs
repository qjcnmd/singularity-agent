//! bash 工具的后端 shell 发现与选择。

use std::path::Path;

/// 根据宿主系统环境选择合适的 Shell 执行命令：
/// Windows 严格使用发现的 Git Bash 或 PATH 中的 bash.exe（绝不回退至 cmd.exe）；
/// Unix 环境优先使用 `/bin/bash`，回退使用 `sh`。
pub(super) fn shell_command(command: &str) -> Result<(String, Vec<String>), String> {
    #[cfg(windows)]
    {
        bash_shell_command(command, find_bash_on_windows())
    }
    #[cfg(not(windows))]
    {
        if Path::new("/bin/bash").exists() {
            return Ok((
                "/bin/bash".to_string(),
                vec!["-c".to_string(), command.to_string()],
            ));
        }
        Ok((
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        ))
    }
}

/// 在进程入口点用与 bash 工具相同的发现规则一次性校验 shell 前置。
pub fn ensure_available() -> Result<(), String> {
    shell_command(":").map(|_| ())
}

#[cfg(windows)]
pub(super) fn bash_shell_command(
    command: &str,
    bash: Option<String>,
) -> Result<(String, Vec<String>), String> {
    let Some(bash) = bash else {
        return Err(
            "Git Bash is required but bash.exe was not found. Install Git for Windows from https://git-scm.com/install/windows, or add the Git bin directory containing bash.exe to PATH."
                .to_string(),
        );
    };
    Ok((bash, vec!["-c".to_string(), command.to_string()]))
}

#[cfg(windows)]
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
