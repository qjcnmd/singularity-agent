//! 提示词装配的唯一 owner：基础人格、工具名单、项目指令与工作目录事实。
//!
//! [`PromptAssembly`] 把「系统提示词 = 基础人格 + 工具名单 + 项目指令 + 工作
//! 目录」的组装收敛到一处：工具名单出自 [`ToolRegistrySnapshot`]（与 schema
//! 同源），项目指令出自 `singularity_core` 的层级合并，工作目录取自 Thread 的
//! 唯一 cwd 形状；预算截断事实经 [`AssembledPrompt::instructions_truncated`]
//! 单向上报给客户端发诊断码，模型侧只看到截断后的指令正文。

use singularity_core::ProjectInstructions;

use crate::tools::ToolRegistrySnapshot;

/// 一次装配的产物：唯一发送给模型的系统提示词与截断事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    pub system_prompt: String,
    /// 项目指令是否因预算超限被截断（客户端据此发稳定诊断码）。
    pub instructions_truncated: bool,
}

/// 系统提示词装配 owner。
pub struct PromptAssembly;

impl PromptAssembly {
    /// 装配一次 turn 的系统提示词：基础人格与工具约定、项目指令、最后的环境事实。
    ///
    /// 工作目录独立成行置于末尾，与项目指令的长文本保持距离以获得最大可见性；
    /// 行尾不带句读，模型复制该路径到命令中时不会连带标点。
    pub fn assemble(
        cwd: &str,
        registry: &ToolRegistrySnapshot,
        instructions: Option<&ProjectInstructions>,
    ) -> AssembledPrompt {
        let mut system_prompt = Self::base_prompt(&registry.prompt_lines());
        let mut instructions_truncated = false;
        if let Some(instructions) = instructions {
            // 项目指令以工作目录为标题、`<INSTRUCTIONS>` 包裹：模型据此把
            // 这段文本识别为"人在这个目录下的要求"，而不是会话内容。
            system_prompt.push_str("\n\n# AGENTS.md instructions for ");
            system_prompt.push_str(cwd);
            system_prompt.push_str("\n\n<INSTRUCTIONS>\n");
            system_prompt.push_str(instructions.content());
            system_prompt.push_str("\n</INSTRUCTIONS>");
            instructions_truncated = instructions.truncated();
        }
        system_prompt.push_str("\n\nCurrent working directory: ");
        system_prompt.push_str(cwd);
        AssembledPrompt {
            system_prompt,
            instructions_truncated,
        }
    }

    /// 基础人格与工作约定提示词：身份句 + 工具名单（每项一行简介）+
    /// Guidelines，与 Pi 的默认提示词同形同序（`pi` 的
    /// `packages/coding-agent/src/core/system-prompt.ts`）。
    ///
    /// Pi 的 Guidelines 由各工具贡献（`promptGuidelines`）再追加两条固定项；
    /// 本仓库工具集固定为六件、贡献者只有 read/write，因此四条直接列在此处，
    /// 不给注册表加 `guidelines` 字段。Pi 的 bash 搜索引导句是有条件的
    /// （仅当 grep/find/ls 缺席时出现），本仓库有 glob/grep，故不产出。
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
             - Use read to examine files instead of cat or sed.\n\
             - Use write only for new files or complete rewrites.\n\
             - Be concise in your responses\n\
             - Show file paths clearly when working with files\n\n\
            Direct system, developer, and user instructions in this prompt take precedence over project instructions."
        )
    }
}
