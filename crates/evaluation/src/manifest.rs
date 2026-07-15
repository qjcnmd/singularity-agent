//! Evaluation manifest 的版本化 schema、任务投影与输入约束。
//!
//! manifest 保留可信执行器使用的内部 `argv`，由 app-server 在模型边界投影为
//! `command` 字符串；验证规则在这里集中维护，避免执行层重复解释 manifest。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use singularity_policy::NetworkAccess;

use crate::{
    Argv, EvaluationError, GitCommit, RelativePath, RemoteRepository, Result,
    TASK_SET_SCHEMA_VERSION, TaskId, ToolName, require_schema_version, validation_error,
};

const COMMAND_TOOL_NAME: &str = "command";
const MAX_COMMAND_TIMEOUT_SECONDS: u64 = 3_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Evaluation task set 的 schema 版本。
pub enum TaskSetSchemaVersion {
    #[serde(rename = "evaluation.task_set/v4")]
    V4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 任务声明覆盖的能力标签。
pub enum EvaluationCapability {
    SingleFileFix,
    MultiFileChange,
    Rust,
    Python,
    Node,
    MixedStack,
    FeatureImplementation,
    FailureDiagnosis,
    ToolCallRecovery,
    RepositoryContext,
    ApprovalResume,
    InterruptionResume,
    RequiredVerification,
    ProviderFailureAttribution,
    SandboxEnforcement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一组可验证的 Evaluation 任务。
pub struct EvaluationTaskSet {
    pub schema_version: TaskSetSchemaVersion,
    pub tasks: Vec<EvaluationTask>,
}

impl EvaluationTaskSet {
    fn validate(&self) -> Result<()> {
        if self.tasks.is_empty() {
            return Err(validation_error(
                "evaluation task set requires at least one task",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            task.validate()?;
            if !task_ids.insert(task.task_id.clone()) {
                return Err(EvaluationError::DuplicateTaskId(task.task_id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个任务的工作区、agent 和 evaluator 定义。
pub struct EvaluationTask {
    pub task_id: TaskId,
    pub description: String,
    pub capabilities: Vec<EvaluationCapability>,
    pub workspace: WorkspaceSpec,
    pub agent: AgentTaskSpec,
    pub evaluator: EvaluatorSpec,
}

impl EvaluationTask {
    fn validate(&self) -> Result<()> {
        if self.description.trim().is_empty() {
            return Err(validation_error(format!(
                "evaluation task {} requires description",
                self.task_id
            )));
        }
        validate_nonempty_unique(&self.task_id, "capabilities", &self.capabilities)?;
        self.workspace.validate(&self.task_id)?;
        self.agent.validate(&self.task_id)?;
        self.evaluator.validate(&self.task_id)
    }

    /// 投影为 AgentLoop 使用的最小任务输入。
    pub fn agent_projection(&self) -> AgentTaskProjection {
        AgentTaskProjection {
            task_id: self.task_id.clone(),
            description: self.description.clone(),
            instructions: self.agent.instructions.clone(),
            allowed_paths: self.agent.allowed_paths.clone(),
            allowed_tools: self.agent.allowed_tools.clone(),
            smoke_commands: self.agent.smoke_commands.clone(),
        }
    }

    fn workspace_plan(&self, manifest_dir: &Path) -> Result<WorkspacePlan> {
        let source = match &self.workspace.source {
            WorkspaceSource::Local { path } => {
                let candidate = manifest_dir.join(path.as_str());
                let path = if candidate.exists() {
                    let path = canonicalize(&candidate)?;
                    if !path.starts_with(manifest_dir) {
                        return Err(validation_error(format!(
                            "evaluation task {} local workspace source escapes the manifest directory: {}",
                            self.task_id,
                            path.display()
                        )));
                    }
                    path
                } else {
                    candidate
                };
                PlannedWorkspaceSource::Local { path }
            }
            WorkspaceSource::RemoteGit { repository, commit } => {
                PlannedWorkspaceSource::RemoteGit {
                    repository: repository.clone(),
                    commit: commit.clone(),
                }
            }
        };
        let public_test_patch = self.evaluator.public_test_patch.clone();
        Ok(WorkspacePlan {
            task_id: self.task_id.clone(),
            capabilities: self.capabilities.clone(),
            source,
            baseline: BaselineStagePlan {
                stage: EvaluationStage::Baseline,
                seed: WorkspaceSeed::TaskSource,
                expectation: CommandExpectation::Failure,
                setup_commands: self.workspace.setup_commands.clone(),
                test_patch: public_test_patch.clone(),
                commands: self.evaluator.baseline.commands.clone(),
            },
            agent: AgentStagePlan {
                stage: EvaluationStage::Agent,
                seed: WorkspaceSeed::TaskSource,
                setup_commands: self.workspace.setup_commands.clone(),
                projection: self.agent_projection(),
            },
            public: VerificationStagePlan {
                stage: EvaluationStage::Public,
                seed: WorkspaceSeed::AgentOutput,
                expectation: CommandExpectation::Success,
                setup_commands: self.workspace.setup_commands.clone(),
                test_patch: public_test_patch,
                commands: self.evaluator.public.commands.clone(),
            },
            hidden: VerificationStagePlan {
                stage: EvaluationStage::Hidden,
                seed: WorkspaceSeed::AgentOutput,
                expectation: CommandExpectation::Success,
                setup_commands: self.workspace.setup_commands.clone(),
                test_patch: self.evaluator.hidden_test_patch.clone(),
                commands: self.evaluator.hidden.commands.clone(),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 任务工作区来源与初始化命令。
pub struct WorkspaceSpec {
    pub source: WorkspaceSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_commands: Vec<CommandSpec>,
}

impl WorkspaceSpec {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        validate_commands(
            task_id,
            "workspace.setup_commands",
            &self.setup_commands,
            false,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
/// 本地或固定 commit 的远程 Git 工作区来源。
pub enum WorkspaceSource {
    Local {
        path: RelativePath,
    },
    RemoteGit {
        repository: RemoteRepository,
        commit: GitCommit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Agent 阶段的指令、路径、工具和 smoke 命令。
pub struct AgentTaskSpec {
    pub instructions: String,
    pub allowed_paths: Vec<RelativePath>,
    pub allowed_tools: Vec<ToolName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub smoke_commands: Vec<CommandSpec>,
}

impl AgentTaskSpec {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        if self.instructions.trim().is_empty() {
            return Err(validation_error(format!(
                "evaluation task {task_id} requires agent.instructions"
            )));
        }
        validate_nonempty_unique(task_id, "agent.allowed_paths", &self.allowed_paths)?;
        validate_nonempty_unique(task_id, "agent.allowed_tools", &self.allowed_tools)?;
        if !self.smoke_commands.is_empty()
            && !self
                .allowed_tools
                .iter()
                .any(|tool| tool.as_str() == COMMAND_TOOL_NAME)
        {
            return Err(validation_error(format!(
                "evaluation task {task_id} agent.smoke_commands requires {COMMAND_TOOL_NAME} in agent.allowed_tools"
            )));
        }
        validate_commands(task_id, "agent.smoke_commands", &self.smoke_commands, false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// baseline、public 和 hidden evaluator 配置。
pub struct EvaluatorSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_test_patch: Option<EvaluatorTestPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_test_patch: Option<EvaluatorTestPatch>,
    pub baseline: EvaluatorStageSpec,
    pub public: EvaluatorStageSpec,
    pub hidden: EvaluatorStageSpec,
}

impl EvaluatorSpec {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        if let Some(test_patch) = &self.public_test_patch {
            test_patch.validate(task_id, "evaluator.public_test_patch")?;
        }
        if let Some(test_patch) = &self.hidden_test_patch {
            test_patch.validate(task_id, "evaluator.hidden_test_patch")?;
        }
        if self.verification_evidence_is_identical() {
            return Err(validation_error(format!(
                "evaluation task {task_id} requires independent public and hidden verification evidence"
            )));
        }
        self.baseline.validate(task_id, "evaluator.baseline")?;
        self.public.validate(task_id, "evaluator.public")?;
        self.hidden.validate(task_id, "evaluator.hidden")
    }

    fn verification_evidence_is_identical(&self) -> bool {
        self.public_test_patch == self.hidden_test_patch
            && self.public.commands.len() == self.hidden.commands.len()
            && self
                .public
                .commands
                .iter()
                .zip(&self.hidden.commands)
                .all(|(public, hidden)| public.argv == hidden.argv && public.cwd == hidden.cwd)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个 evaluator 阶段的命令列表。
pub struct EvaluatorStageSpec {
    pub commands: Vec<CommandSpec>,
}

impl EvaluatorStageSpec {
    fn validate(&self, task_id: &TaskId, field: &str) -> Result<()> {
        validate_commands(task_id, field, &self.commands, true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// evaluator 使用的受限命令描述。
pub struct CommandSpec {
    pub argv: Argv,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<RelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(
        default = "denied_network_access",
        skip_serializing_if = "network_access_is_denied"
    )]
    pub network_access: NetworkAccess,
}

fn denied_network_access() -> NetworkAccess {
    NetworkAccess::Denied
}

fn network_access_is_denied(network_access: &NetworkAccess) -> bool {
    *network_access == NetworkAccess::Denied
}

impl CommandSpec {
    fn validate(&self, task_id: &TaskId, field: &str) -> Result<()> {
        if self.timeout_seconds == Some(0) {
            return Err(validation_error(format!(
                "evaluation task {task_id} {field} timeout_seconds must be greater than zero"
            )));
        }
        if self
            .timeout_seconds
            .is_some_and(|timeout| timeout > MAX_COMMAND_TIMEOUT_SECONDS)
        {
            return Err(validation_error(format!(
                "evaluation task {task_id} {field} timeout_seconds must not exceed {MAX_COMMAND_TIMEOUT_SECONDS}"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// evaluator 测试补丁的格式。
pub enum PatchFormat {
    UnifiedDiff,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// evaluator 阶段应用的测试补丁。
pub struct EvaluatorTestPatch {
    pub format: PatchFormat,
    content: String,
}

impl EvaluatorTestPatch {
    /// 返回未经修改的补丁正文。
    pub fn content(&self) -> &str {
        &self.content
    }

    fn validate(&self, task_id: &TaskId, field: &str) -> Result<()> {
        if self.content.trim().is_empty() {
            return Err(validation_error(format!(
                "evaluation task {task_id} {field}.content must not be empty"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
/// 已校验并绑定 manifest 目录的 Evaluation 定义。
pub struct EvaluationManifest {
    task_set: EvaluationTaskSet,
    manifest_dir: PathBuf,
}

impl EvaluationManifest {
    /// 从 JSON 和 manifest 所在目录加载并校验定义。
    pub fn from_json_str(json: &str, manifest_dir: impl AsRef<Path>) -> Result<Self> {
        require_schema_version(json, TASK_SET_SCHEMA_VERSION)?;
        let manifest_dir = canonicalize(manifest_dir.as_ref())?;
        if !manifest_dir.is_dir() {
            return Err(validation_error(format!(
                "evaluation manifest directory is not a directory: {}",
                manifest_dir.display()
            )));
        }
        let task_set: EvaluationTaskSet = serde_json::from_str(json)?;
        task_set.validate()?;
        Ok(Self {
            task_set,
            manifest_dir,
        })
    }

    /// 从文件加载 Evaluation manifest。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = canonicalize(path.as_ref())?;
        let json = fs::read_to_string(&path).map_err(|source| EvaluationError::Io {
            path: path.clone(),
            source,
        })?;
        let manifest_dir = path.parent().ok_or_else(|| {
            validation_error(format!(
                "evaluation manifest has no parent directory: {}",
                path.display()
            ))
        })?;
        Self::from_json_str(&json, manifest_dir)
    }

    /// 返回原始 task set。
    pub fn task_set(&self) -> &EvaluationTaskSet {
        &self.task_set
    }

    /// 返回 manifest 的规范化目录。
    pub fn manifest_dir(&self) -> &Path {
        &self.manifest_dir
    }

    /// 为指定任务生成四阶段工作区计划。
    pub fn workspace_plan(&self, task_id: &TaskId) -> Result<WorkspacePlan> {
        self.task_set
            .tasks
            .iter()
            .find(|task| &task.task_id == task_id)
            .ok_or_else(|| EvaluationError::TaskNotFound(task_id.clone()))?
            .workspace_plan(&self.manifest_dir)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Evaluation 执行阶段。
pub enum EvaluationStage {
    Baseline,
    Agent,
    Public,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 阶段工作区的来源种子。
pub enum WorkspaceSeed {
    TaskSource,
    AgentOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 阶段命令的预期结果。
pub enum CommandExpectation {
    Success,
    Failure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// 已解析的工作区来源计划。
pub enum PlannedWorkspaceSource {
    Local {
        path: PathBuf,
    },
    RemoteGit {
        repository: RemoteRepository,
        commit: GitCommit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// 单个任务的完整执行计划。
pub struct WorkspacePlan {
    pub task_id: TaskId,
    pub capabilities: Vec<EvaluationCapability>,
    pub source: PlannedWorkspaceSource,
    pub baseline: BaselineStagePlan,
    pub agent: AgentStagePlan,
    pub public: VerificationStagePlan,
    pub hidden: VerificationStagePlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// baseline 阶段计划。
pub struct BaselineStagePlan {
    pub stage: EvaluationStage,
    pub seed: WorkspaceSeed,
    pub expectation: CommandExpectation,
    pub setup_commands: Vec<CommandSpec>,
    pub test_patch: Option<EvaluatorTestPatch>,
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// Agent 阶段计划。
pub struct AgentStagePlan {
    pub stage: EvaluationStage,
    pub seed: WorkspaceSeed,
    pub setup_commands: Vec<CommandSpec>,
    pub projection: AgentTaskProjection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// public 或 hidden 验证阶段计划。
pub struct VerificationStagePlan {
    pub stage: EvaluationStage,
    pub seed: WorkspaceSeed,
    pub expectation: CommandExpectation,
    pub setup_commands: Vec<CommandSpec>,
    pub test_patch: Option<EvaluatorTestPatch>,
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 传给 AgentLoop 的任务输入投影。
pub struct AgentTaskProjection {
    pub task_id: TaskId,
    pub description: String,
    pub instructions: String,
    pub allowed_paths: Vec<RelativePath>,
    pub allowed_tools: Vec<ToolName>,
    pub smoke_commands: Vec<CommandSpec>,
}

fn validate_commands(
    task_id: &TaskId,
    field: &str,
    commands: &[CommandSpec],
    required: bool,
) -> Result<()> {
    if required && commands.is_empty() {
        return Err(validation_error(format!(
            "evaluation task {task_id} {field}.commands must not be empty"
        )));
    }
    for (index, command) in commands.iter().enumerate() {
        command.validate(task_id, &format!("{field}[{index}]"))?;
    }
    Ok(())
}

fn validate_nonempty_unique<T>(task_id: &TaskId, field: &str, values: &[T]) -> Result<()>
where
    T: Ord + Clone,
{
    if values.is_empty() {
        return Err(validation_error(format!(
            "evaluation task {task_id} {field} must not be empty"
        )));
    }
    let unique = values.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return Err(validation_error(format!(
            "evaluation task {task_id} {field} contains duplicates"
        )));
    }
    Ok(())
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|source| EvaluationError::Io {
        path: path.to_path_buf(),
        source,
    })
}
