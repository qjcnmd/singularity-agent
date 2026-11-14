//! 上下文模块：每个模块自带提示词片段、工具定义和估算 Token，任务创建时可以关掉其中一部分。
//!
//! 模块之间没有引用：基础模块不提任何可选工具，各工具的一行用法跟着工具走；
//! 一个模块关闭，它的提示词和工具一起消失。新建任务页的胶囊、运行状态面板和每轮请求
//! 都由 `Assembly` 一处生成，数字和实际发送的内容一致。

use std::sync::Arc;

use singularity_core::skills::SkillCatalog;
use singularity_core::{InstructionFile, estimate_tokens_of, estimate_tool_tokens};
use singularity_mcp::McpDiscovery;
use singularity_model::Message;
use singularity_protocol::{ContextGroup, ContextModule};

use crate::tools::{self, ToolRegistrySnapshot};

const SYSTEM_PROMPT: &str = "system";
const GLOBAL_INSTRUCTIONS: &str = "global-instructions";
const PROJECT_INSTRUCTIONS: &str = "project-instructions";
const BASH: &str = "bash";
const TASKS: &str = "tasks";
const READ: &str = "read";
const EDIT: &str = "edit";
const VIEW_IMAGE: &str = "view-image";
const QUESTION: &str = "ask-user-question";
const SKILLS: &str = "skills";
pub(super) const MCP_PREFIX: &str = "mcp:";

fn mcp_module_id(server_id: &str) -> String {
    format!("{MCP_PREFIX}{server_id}")
}

/// 组装一次请求前需要的外部输入。
pub(crate) struct ModuleInputs<'a> {
    pub cwd: &'a str,
    pub instructions: Vec<InstructionFile>,
    pub skills: SkillCatalog,
    pub mcp: McpDiscovery,

    pub disabled: &'a [String],
}

/// 一轮请求的全部定义：Developer 与文件指令消息、工具、模块清单。
pub(crate) struct Assembly {
    pub messages: Vec<Message>,
    pub registry: ToolRegistrySnapshot,
    pub modules: Vec<ContextModule>,
}

/// 收集每个模块的内容。模块清单包含关掉的模块，让页面显示完整胶囊；只有开着的模块
/// 向请求贡献提示词和工具。
struct Collector<'a> {
    disabled: &'a [String],
    modules: Vec<ContextModule>,
    /// 开着的模块按顺序提供的 Developer 片段。
    developer: Vec<String>,
}

impl Collector<'_> {
    fn push(
        &mut self,
        id: &str,
        group: ContextGroup,
        label: &str,
        detail: String,
        tokens: u64,
        locked: bool,
    ) -> bool {
        let enabled = locked || !self.disabled.iter().any(|disabled| disabled == id);
        self.modules.push(ContextModule {
            id: id.to_string(),
            group,
            label: label.to_string(),
            detail,
            tokens,
            locked,
            enabled,
        });
        enabled
    }
}

pub(crate) fn assemble(inputs: ModuleInputs<'_>) -> Assembly {
    let ModuleInputs { cwd, instructions, skills, mcp, disabled } = inputs;
    let mut collector = Collector {
        disabled,
        modules: Vec::new(),
        developer: Vec::new(),
    };
    let mut registry = ToolRegistrySnapshot::default();
    let mut file_messages = Vec::new();
    let mut tool_guidelines = Vec::new();

    // 指令：系统提示词在下面统一生成（要等工具用法收齐），先占位保证胶囊顺序。
    let system_index = collector.modules.len();
    collector.push(
        SYSTEM_PROMPT,
        ContextGroup::Instructions,
        "系统提示词",
        "身份、平台、工作目录和使用工具的基本做法".into(),
        0,
        true,
    );

    for (id, label, global) in [
        (GLOBAL_INSTRUCTIONS, "全局指令", true),
        (PROJECT_INSTRUCTIONS, "项目指令", false),
    ] {
        let files: Vec<_> = instructions.iter().filter(|file| file.global == global).collect();
        let text = files
            .iter()
            .map(|file| format!("# Instructions from {}\n\n{}", file.path.display(), file.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        let detail = if files.is_empty() {
            "没有找到这类指令文件".to_string()
        } else {
            files
                .iter()
                .map(|file| singularity_core::display_path(&file.path))
                .collect::<Vec<_>>()
                .join("\n")
        };
        if collector.push(id, ContextGroup::Instructions, label, detail, estimate_tokens_of(&text), false)
            && !files.is_empty()
        {
            file_messages.push(text);
        }
    }

    // 工具：bash 和后台命令锁定；其他内置工具各自一个模块。
    let bash = tools::bash_tool();
    let task_tools = tools::task_tools();
    collector.push(
        BASH,
        ContextGroup::Tools,
        "bash",
        "在工作目录执行命令".into(),
        estimate_tool_tokens([&bash.0]),
        true,
    );
    registry.add_builtin([bash]);
    let task_names = task_tools.iter().map(|(schema, _)| schema.name.as_str()).collect::<Vec<_>>().join("、");
    let task_tokens = estimate_tool_tokens(task_tools.iter().map(|(schema, _)| schema));
    collector.push(
        TASKS,
        ContextGroup::Tools,
        "task_*",
        format!("{task_names}：查看、读取输出和停止后台命令"),
        task_tokens,
        true,
    );
    registry.add_builtin(task_tools);

    let optional_tools = [
        (
            EDIT,
            "edit",
            "精确替换文本文件内容",
            tools::edit_tool(),
            Some(
                "Use edit for targeted changes to existing text files; combine independent changes to one file in a single call.",
            ),
        ),
        (READ, "read", "读取文本文件", tools::read_tool(), Some("Use read for text files.")),
        (
            VIEW_IMAGE,
            "view_image",
            "查看磁盘上的图片文件",
            tools::view_image_tool(),
            Some("Use view_image for image files."),
        ),
        (QUESTION, "ask_user_question", "向用户提问", tools::question_tool(), None),
    ];
    for (id, label, detail, tool, guideline) in optional_tools {
        let tokens = estimate_tool_tokens([&tool.0]);
        if collector.push(id, ContextGroup::Tools, label, detail.into(), tokens, false) {
            registry.add_builtin([tool]);
            tool_guidelines.extend(guideline);
        }
    }

    // Skills：目录提示加 skill 工具，不依赖 read 和 bash。
    let skill_prompt = skills.prompt();
    let skill_tokens = estimate_tokens_of(&skill_prompt) + estimate_tool_tokens([&tools::skill_schema()]);
    let skill_detail = if skill_prompt.is_empty() {
        "没有可公布给模型的技能".to_string()
    } else {
        skills.model_skills().map(|skill| skill.name.as_str()).collect::<Vec<_>>().join("、")
    };
    if collector.push(
        SKILLS,
        ContextGroup::Skills,
        "Skills",
        skill_detail,
        if skill_prompt.is_empty() { 0 } else { skill_tokens },
        false,
    ) && !skill_prompt.is_empty()
    {
        collector.developer.push(skill_prompt);
        registry.set_skills(Arc::new(skills));
    }

    // MCP：每个服务器一个模块，工具、服务器说明和连接错误一起开关。本任务关掉的服务器不连接。
    if let Some(error) = mcp.error {
        collector.developer.push(format!("Unavailable MCP configuration:\n{error}"));
    }
    for server in mcp.servers {
        let detail = match &server.error {
            Some(error) => format!("连接失败：{error}"),
            None => server
                .tools
                .iter()
                .map(|tool| tool.definition.name.as_str())
                .collect::<Vec<_>>()
                .join("、"),
        };
        if collector.push(
            &mcp_module_id(&server.id),
            ContextGroup::Mcp,
            &server.id,
            detail,
            server.tokens,
            false,
        ) {
            collector.developer.extend(server.prompt);
            registry.add_mcp_tools(server.tools);
        }
    }

    let system = assemble_developer_instructions(cwd, &tool_guidelines);
    collector.modules[system_index].tokens = estimate_tokens_of(&system);
    let mut messages = vec![Message::developer(
        [system].into_iter().chain(collector.developer).collect::<Vec<_>>().join("\n\n"),
    )];
    if !file_messages.is_empty() {
        messages.push(file_instruction_message(&file_messages.join("\n\n")));
    }
    Assembly {
        messages,
        registry,
        modules: collector.modules,
    }
}

/// 新建任务页的胶囊清单：指令和技能按当前项目读取，工具是固定的，MCP 服务器取自
/// 已有连接留下的状态，不连接服务器。口径与 `assemble` 相同，全部开着。
pub fn preview(
    cwd: &str,
    instructions: Vec<InstructionFile>,
    skills: SkillCatalog,
    servers: Vec<singularity_protocol::McpServerState>,
) -> Vec<ContextModule> {
    let mcp = McpDiscovery::default();
    let mut modules = assemble(ModuleInputs {
        cwd,
        instructions,
        skills,
        mcp,
        disabled: &[],
    })
    .modules;
    modules.extend(servers.into_iter().map(|server| {
        let id = server.server.server_id;
        let detail = match (&server.status, server.error) {
            (singularity_protocol::McpConnectionStatus::NotConnected, _) => {
                "尚未连接，创建任务时才会连接".to_string()
            }
            (_, Some(error)) => format!("连接失败：{error}"),
            (_, None) => server.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>().join("、"),
        };
        ContextModule {
            id: mcp_module_id(&id),
            group: ContextGroup::Mcp,
            label: id,
            detail,
            tokens: server.tokens,
            locked: false,
            enabled: true,
        }
    }));
    modules
}

/// 生成 Developer 指令；`tool_guidelines` 是本任务开着的工具模块各自提供的一行用法，
/// 文件指令和技能列表由请求组装代码另外添加。
fn assemble_developer_instructions(cwd: &str, tool_guidelines: &[&str]) -> String {
    let tool_guidelines: String = tool_guidelines.iter().map(|line| format!("- {line}\n")).collect();
    // 写明平台和 shell，否则模型可能在 Git Bash 里写 cmd 语法（如 `cd /d`、`2>nul`），
    // 输出被重定向丢掉。
    format!(
        "You are an expert coding assistant operating inside Singularity, a coding agent \
             harness. You help users by reading files, executing commands, editing code, and \
             writing new files.\n\n\
             Guidelines:\n\
             {tool_guidelines}\
             - Use bash with rg for content searches and rg --files for file discovery; use Python or other commands for file writes, byte ranges, and structured processing.\n\
             - Be concise in your responses\n\
             - Show file paths clearly when working with files\n\n\
             Environment:\n- Platform: windows\n\
             - Command shell: bash (Git Bash on Windows).\n\n\
             Current working directory: {cwd}"
    )
}

fn file_instruction_message(instructions: &str) -> Message {
    // 转义正文里的结束标签，否则模型会以为指令在那里结束。
    let instructions = instructions.replace("</file-instructions>", "<\\/file-instructions>");
    Message::user(format!(
        "<file-instructions>\nThese global and project file instructions apply to the current workspace. More specific files take precedence. Direct user instructions take precedence over these file instructions.\n{instructions}\n</file-instructions>"
    ))
}
