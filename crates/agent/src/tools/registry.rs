use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use ::mcp::McpTool;
use app_core::ToolDefinition;
use app_core::skills::SkillCatalog;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::edit;
use super::mcp;
use super::read;
use super::skill;
use super::view_image;
use super::web;
use super::write;

pub(crate) const DEFAULT_MAX_LINES: usize = 2000;
pub(crate) const DEFAULT_MAX_BYTES: usize = 50 * 1024;

pub(crate) const fn default_max_kb() -> usize {
    DEFAULT_MAX_BYTES / 1024
}

pub(crate) fn default_cap_summary() -> String {
    format!("{DEFAULT_MAX_LINES} lines or {}KB", default_max_kb())
}

#[derive(Debug)]
pub(crate) struct ToolExecution {
    pub content: String,
    pub display: Option<Box<ToolDisplay>>,
    pub images: Vec<crate::image::InputImage>,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    pub read_source: Option<protocol::ReadSource>,
    pub file_created: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct ToolDisplay {
    pub output: String,
    pub background_task_id: Option<String>,
}

impl ToolExecution {
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            display: None,
            images: Vec::new(),
            is_error: false,
            duration_ms: None,
            read_source: None,
            file_created: None,
        }
    }

    pub(crate) fn with_read_source(mut self, source: protocol::ReadSource) -> Self {
        self.read_source = Some(source);
        self
    }
}

pub(crate) struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub output_directory: std::path::PathBuf,
    pub signal: CancellationToken,
    pub bash_tasks: Arc<bash::BashTasks>,
}

pub(crate) enum PreparedTool {
    Edit(edit::EditArgs),
    Write(write::WriteArgs),
    Read(read::ReadArgs),
    ViewImage(view_image::ViewImageArgs),
    Skill(skill::SkillArgs, Arc<SkillCatalog>),
    Question(super::question::QuestionArgs),
    Bash(bash::BashArgs),
    TaskList,
    TaskOutput(bash::TaskIdArgs),
    TaskStop(bash::TaskIdArgs),
    McpSchema(Vec<McpTool>),
    Mcp(McpTool, serde_json::Map<String, Value>),
    WebSearch(web::WebSearchArgs, Arc<model::WebSearchAccess>),
    WebFetch(web::WebFetchArgs, Arc<model::WebSearchAccess>),
}

impl PreparedTool {
    pub(crate) async fn execute<F: Future<Output = ()> + Send>(
        self,
        context: ToolContext,
        on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        let ToolContext {
            cwd,
            output_directory,
            signal,
            bash_tasks,
        } = context;
        match self {
            Self::Edit(args) => Box::pin(edit::execute(args, cwd, signal)).await,
            Self::Write(args) => Box::pin(write::execute(args, cwd, signal)).await,
            Self::McpSchema(tools) => mcp::execute_schema(&tools),
            Self::Mcp(tool, args) => Box::pin(mcp::execute(tool, args, output_directory, &signal)).await,
            Self::WebSearch(args, access) => {
                Box::pin(web::execute_search(args, access, output_directory, &signal)).await
            }
            Self::WebFetch(args, access) => {
                Box::pin(web::execute_fetch(args, access, output_directory, &signal)).await
            }
            Self::Bash(args) => {
                Box::pin(bash_tasks.execute(args, cwd, output_directory, signal, on_update)).await
            }
            Self::TaskStop(args) => Box::pin(bash_tasks.tool_stop(&args.task_id)).await,
            Self::TaskList | Self::TaskOutput(_) | Self::Read(_) | Self::ViewImage(_) | Self::Skill(_, _) => {
                tokio::task::spawn_blocking(move || match self {
                    Self::TaskList => bash_tasks.tool_list(),
                    Self::TaskOutput(args) => bash_tasks.output(&args.task_id, &signal),
                    Self::Read(args) => read::execute(&args, &cwd, &signal),
                    Self::ViewImage(args) => view_image::execute(&args, &cwd, &signal),
                    Self::Skill(args, catalog) => skill::execute(&args, &catalog),
                    _ => unreachable!("non-blocking tool handled above"),
                })
                .await
                .expect("tool worker completes while the runtime is running")
            }
            Self::Question(_) => unreachable!("questions execute on the turn control plane"),
        }
    }
}

pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

type ToolParser = fn(&Value) -> Result<PreparedTool, ToolExecution>;

pub(crate) type BuiltinTool = (ToolDefinition, ToolParser);

pub(crate) fn bash_tool() -> BuiltinTool {
    (bash::spec(), |args| {
        deserialize_args_or_error::<bash::BashArgs>(args)?.validate().map(PreparedTool::Bash)
    })
}

pub(crate) fn task_tools() -> Vec<BuiltinTool> {
    vec![
        (bash::task_list_spec(), |args| {
            deserialize_args_or_error::<bash::TaskListArgs>(args).map(|_| PreparedTool::TaskList)
        }),
        (bash::task_output_spec(), |args| {
            deserialize_args_or_error(args).map(PreparedTool::TaskOutput)
        }),
        (bash::task_stop_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskStop)),
    ]
}

pub(crate) fn read_tool() -> BuiltinTool {
    (read::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Read))
}

pub(crate) fn edit_tool() -> BuiltinTool {
    (edit::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Edit))
}

pub(crate) fn write_tool() -> BuiltinTool {
    (write::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Write))
}

pub(crate) fn view_image_tool() -> BuiltinTool {
    (view_image::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::ViewImage))
}

pub(crate) fn question_tool() -> BuiltinTool {
    (super::question::spec(), |args| {
        deserialize_args_or_error::<super::question::QuestionArgs>(args)?
            .validate()
            .map(PreparedTool::Question)
    })
}

#[derive(Default)]
pub(crate) struct ToolRegistrySnapshot {
    tools: Vec<BuiltinTool>,
    mcp_servers: BTreeMap<String, Vec<McpTool>>,
    skills: Option<Arc<SkillCatalog>>,
    web: Option<Arc<model::WebSearchAccess>>,
}

impl ToolRegistrySnapshot {
    pub(crate) fn add_builtin(&mut self, tools: impl IntoIterator<Item = BuiltinTool>) {
        self.tools.extend(tools);
    }

    pub(crate) fn set_skills(&mut self, catalog: Arc<SkillCatalog>) {
        self.skills = Some(catalog);
    }

    pub(crate) fn set_web(&mut self, access: Arc<model::WebSearchAccess>) {
        self.web = Some(access);
    }

    pub(crate) fn add_mcp_server(&mut self, server_id: String, tools: Vec<McpTool>) {
        if !tools.is_empty() {
            self.mcp_servers.insert(server_id, tools);
        }
    }

    pub(crate) fn provider_schemas(&self) -> Vec<ToolDefinition> {
        let web_tools =
            self.web.as_ref().map(|_| [web::search_spec(), web::fetch_spec()]).into_iter().flatten();
        let mcp_proxies = (!self.mcp_servers.is_empty())
            .then(|| [mcp::schema_spec(), mcp::call_spec()])
            .into_iter()
            .flatten();
        self.tools
            .iter()
            .map(|(schema, _)| schema.clone())
            .chain(web_tools)
            .chain(self.skills.iter().map(|_| skill::spec()))
            .chain(mcp_proxies)
            .collect()
    }

    pub(crate) fn preflight(&self, name: &str, args: &Value) -> Result<PreparedTool, ToolExecution> {
        if !self.mcp_servers.is_empty() {
            if name == mcp::MCP_SCHEMA_TOOL_NAME {
                let args: mcp::McpSchemaArgs = deserialize_args_or_error(args)?;
                let tools = mcp::lookup_tools(&self.mcp_servers, &args.server, &args.tools)?;
                return Ok(PreparedTool::McpSchema(tools));
            }
            if name == mcp::MCP_CALL_TOOL_NAME {
                let args: mcp::McpCallArgs = deserialize_args_or_error(args)?;
                let tool = mcp::lookup_tool(&self.mcp_servers, &args.server, &args.tool)?;
                return Ok(PreparedTool::Mcp(tool.clone(), args.arguments));
            }
        }
        if let Some(access) = &self.web {
            if name == web::WEB_SEARCH_TOOL_NAME {
                return deserialize_args_or_error::<web::WebSearchArgs>(args)?
                    .validate()
                    .map(|args| PreparedTool::WebSearch(args, Arc::clone(access)));
            }
            if name == web::WEB_FETCH_TOOL_NAME {
                return deserialize_args_or_error::<web::WebFetchArgs>(args)?
                    .validate()
                    .map(|args| PreparedTool::WebFetch(args, Arc::clone(access)));
            }
        }
        if name == "skill"
            && let Some(catalog) = &self.skills
        {
            return deserialize_args_or_error(args)
                .map(|args| PreparedTool::Skill(args, Arc::clone(catalog)));
        }
        let (_, parse) = self
            .tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .ok_or_else(|| error_result(format!("tool execution failed: unknown tool: {name}")))?;
        parse(args)
    }
}

pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    let mut execution = ToolExecution::text(message);
    execution.is_error = true;
    execution
}

pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(args: &Value) -> Result<T, ToolExecution> {
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}

pub(crate) fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(<Option<T> as serde::Deserialize>::deserialize(deserializer)?.unwrap_or_default())
}
