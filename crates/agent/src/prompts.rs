//! 系统提示词装配：固定行为、工具约定和当前目录；文件指令由上下文注入。

use crate::tools::ToolRegistrySnapshot;

/// 系统提示词只承载固定行为和环境；文件指令由 Context 独立注入。
pub struct PromptAssembly;
impl PromptAssembly {
    pub fn assemble(cwd: &str, registry: &ToolRegistrySnapshot) -> String {
        let mut prompt = Self::base_prompt(&registry.prompt_lines());
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
}
