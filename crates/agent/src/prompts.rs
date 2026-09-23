//! Harness 的 Developer 指令：固定行为、工具约定、环境事实与当前工作目录。

use crate::tools::ToolRegistrySnapshot;

/// 文件指令与 Skill 目录提示由 Agent 请求装配。
pub fn assemble_developer_instructions(cwd: &str, registry: &ToolRegistrySnapshot) -> String {
    let mut prompt = base_prompt(&registry.prompt_lines());
    // 平台和命令 shell 是稳定事实，必须写清楚：不说明时模型会自己猜方言（比如在 Git Bash
    // 上写 cmd 的 `cd /d`、`2>nul`），猜错的代价是输出被重定向吞掉，模型还查不出来。
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
             - Show file paths clearly when working with files"
    )
}
