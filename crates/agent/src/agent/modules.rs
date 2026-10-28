//! 上下文模块：每个模块自带提示词片段、工具定义和估算 Token，任务创建时可以关掉其中一部分。
//!
//! 模块之间没有引用：基础模块不提任何可选工具，各工具的一行用法跟着工具走；
//! 一个模块关闭，它的提示词和工具一起消失。新建任务页的胶囊、运行状态面板和每轮请求
//! 都由 `Assembly` 一处生成，数字和实际发送的内容一致。

use std::sync::Arc;

use singularity_core::skills::SkillCatalog;
use singularity_core::{InstructionFile, estimate_tokens_of};
use singularity_mcp::McpDiscovery;
use singularity_model::{Message, ModelToolSchema};
use singularity_protocol::{ContextGroup, ContextModule};

use crate::tools::{self, ToolRegistrySnapshot};

pub const SYSTEM_PROMPT: &str = "system";
pub const GLOBAL_INSTRUCTIONS: &str = "global-instructions";
pub const PROJECT_INSTRUCTIONS: &str = "project-instructions";
pub const BASH: &str = "bash";
pub const TASKS: &str = "tasks";
pub const READ: &str = "read";
pub const VIEW_IMAGE: &str = "view-image";
pub const QUESTION: &str = "ask-user-question";
pub const SKILLS: &str = "skills";
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
    /// 创建任务时关闭的模块 ID。
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
    /// 登记一个模块，返回它是否开着。
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

fn tools_tokens(tools: &[ModelToolSchema]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    estimate_tokens_of(&serde_json::to_string(tools).expect("tool schemas are serializable")) + 4
}

fn names(tools: &[ModelToolSchema]) -> String {
    tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>().join("、")
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

    // 工具：bash 和后台命令锁定；read、view_image、提问各自一个模块。
    let bash = tools::bash_tool();
    let task_tools = tools::task_tools();
    let bash_schemas = [bash.0.clone()];
    collector.push(
        BASH,
        ContextGroup::Tools,
        "bash",
        "在工作目录执行命令".into(),
        tools_tokens(&bash_schemas),
        true,
    );
    registry.add_builtin([bash]);
    let task_schemas: Vec<_> = task_tools.iter().map(|(schema, _)| schema.clone()).collect();
    collector.push(
        TASKS,
        ContextGroup::Tools,
        "task_*",
        format!("{}：查看、读取输出和停止后台命令", names(&task_schemas)),
        tools_tokens(&task_schemas),
        true,
    );
    registry.add_builtin(task_tools);

    let optional_tools = [
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
        let schemas = [tool.0.clone()];
        if collector.push(id, ContextGroup::Tools, label, detail.into(), tools_tokens(&schemas), false) {
            registry.add_builtin([tool]);
            tool_guidelines.extend(guideline);
        }
    }

    // Skills：目录提示加 skill 工具，不依赖 read 和 bash。
    let skill_prompt = skills.prompt();
    let skill_schemas = [tools::skill_schema()];
    let skill_tokens = estimate_tokens_of(&skill_prompt) + tools_tokens(&skill_schemas);
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
            None => server.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>().join("、"),
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

    let system = crate::prompts::assemble_developer_instructions(cwd, &tool_guidelines);
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

fn file_instruction_message(instructions: &str) -> Message {
    // 转义正文里的结束标签，否则模型会以为指令在那里结束。
    let instructions = instructions.replace("</file-instructions>", "<\\/file-instructions>");
    Message::user(format!(
        "<file-instructions>\nThese global and project file instructions apply to the current workspace. More specific files take precedence. Direct user instructions take precedence over these file instructions.\n{instructions}\n</file-instructions>"
    ))
}
