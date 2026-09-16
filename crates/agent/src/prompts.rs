//! 系统提示词装配：固定行为、工具约定、平台与命令 shell 等环境事实，以及当前目录；文件指令由上下文注入。

use crate::tools::ToolRegistrySnapshot;

/// 系统提示词只承载固定行为和环境；文件指令由 Context 独立注入。
pub fn assemble_system_prompt(cwd: &str, registry: &ToolRegistrySnapshot) -> String {
    let mut prompt = base_prompt(&registry.prompt_lines());
    // 平台与命令 shell 是稳定事实：不说明时模型会自行猜测方言（例如在 Git Bash
    // 上写 cmd 的 `cd /d`、`2>nul`），而猜错的代价是被重定向吞掉、无法自查。
    prompt.push_str("\n\nEnvironment:\n- Platform: ");
    prompt.push_str(std::env::consts::OS);
    prompt.push_str("\n- Command shell: bash (Git Bash on Windows).");
    prompt.push_str("\n\nCurrent working directory: ");
    prompt.push_str(cwd);
    prompt
}

fn base_prompt(tools: &[(&str, &str)]) -> String {
    let available_tools = tools
        .iter()
        .map(|(name, snippet)| format!("- {name}: {snippet}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are an expert coding assistant operating inside Singularity, a coding agent \
             harness. You help users by reading files, executing commands, editing code, and \
             writing new files.\n\n\
             Available tools:\n{available_tools}\n\n\
             Guidelines:\n\
             - Use read for text files; use bash for byte ranges or structured processing.\n\
             - Use write only for new files or complete rewrites.\n\
             - Be concise in your responses\n\
             - Show file paths clearly when working with files\n\n\
            Direct system, developer, and user instructions in this prompt take precedence over project instructions."
    )
}
