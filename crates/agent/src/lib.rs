#![forbid(unsafe_code)]

//! 负责模型 turn、tool 执行、approval 检查点和完成校验的 `AgentLoop` 状态机。
//!
//! loop 将模型提供方可见历史与规范化可执行调用分离，所有副作用都经由 `ToolBroker`，
//! 并在完成或恢复不变量不满足时拒绝继续执行。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::ControlFlow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, contains_sensitive_text};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelError, ModelErrorCategory,
    ModelErrorKind, ModelMessage, ModelPreferences, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelToolSchema, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage, Provider,
    ProviderAttemptMetadata, ProviderCapabilityMetadata, ProviderDiagnostic, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ToolChoiceMode, is_strict_tool_schema_compatible,
    provider_error_response, validate_model_request_with_capabilities,
    validate_model_turn_response,
};
use singularity_policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, NetworkAccess, PermissionDecision,
    PermissionDecisionCause, PermissionDecisionOutcome, PermissionOperation, PermissionProfile,
    PermissionProfileName, PermissionRequest, PolicyEngine,
};
use singularity_tools::{
    COMMAND_TOOL as TOOL_COMMAND, CommandToolInput, EDIT_TOOL as TOOL_EDIT, EditToolInput,
    GREP_TOOL as TOOL_GREP, GrepToolInput, LIST_TOOL as TOOL_LIST, ListToolInput,
    PATCH_TOOL as TOOL_PATCH, READ_TOOL as TOOL_READ, ReadToolInput, SandboxFilesystemMode,
    SandboxNetworkMode, ToolBroker, ToolBrokerDecision, ToolCallRequest, ToolExecutionMode,
    ToolFailureKind, ToolInputValidationError, ToolOutput, ToolResult, ToolSpec, WorkspacePatch,
    WorkspaceToolError, WorkspaceTools, command_script_scope_digest_with_policy,
    command_script_scope_resource_with_policy, is_protected_path,
};
use thiserror::Error;

const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 16;
const MAX_PARALLEL_READ_TOOL_CALLS: u32 = 8;
const APPROVAL_CHECKPOINT_VERSION: u32 = 1;
const AGENT_DEVELOPER_INSTRUCTIONS: &str = "You are a coding agent working in the current workspace. Inspect real files before making claims. Use tools for changes, write only inside the workspace, and run verification after the last mutation. Report only completed work and verification. Read-only questions need no changes or verification. For multi-step work, keep a concise update_plan plan; revise it when evidence or failure changes the approach, and complete it before the final answer. Skip plans for simple read-only or single-step work. Tools can be submitted only through native structured tool calls; ordinary text is never executed. Match registered tool schemas exactly and use typed tool results to correct parameters.";
const APPROXIMATE_ASCII_CHARS_PER_TOKEN: usize = 4;
const USER_MESSAGE_ROLE: &str = "user";
const ASSISTANT_MESSAGE_ROLE: &str = "assistant";
const MODEL_MESSAGE_FRAMING_TOKENS: u32 = 4;
const MODEL_REQUEST_FIXED_OVERHEAD_TOKENS: u32 = 256;
const EMPTY_FINAL_ANSWER_ERROR: &str = "empty final answer";
const CURRENT_TURN_CONTEXT_OVERFLOW_ERROR: &str = "current turn exceeds the model context budget";
const MODEL_REQUEST_CONTEXT_OVERFLOW_ERROR: &str = "model request exceeds the model context budget";
const MODEL_RESPONSE_VALIDATION_ERROR: &str = "model response validation failed";
const POST_MUTATION_VERIFICATION_REQUIRED: &str =
    "completion gate rejected final answer: verification required after workspace mutation";
const REPAIRABLE_TOOL_ERROR_CODES: [&str; 8] = [
    "invalid_tool_arguments",
    "invalid_tool_input",
    "expected_content_missing",
    "tool_read_failed",
    "binary_pattern",
    "command_exit_nonzero",
    "command_tests_failed",
    "command_build_failed",
];
const TOOL_SELECTION_FAILURE_GROUP: &str = "tool_selection";
const TOOL_SELECTION_FAILURE_PREFIX: &str = "tool_selection:";
/// AgentLoop 内部使用的计划更新 tool 名称。
pub const UPDATE_PLAN_TOOL: &str = "update_plan";
// 仅供 provider history 使用；该占位名称不会注册，也不会执行。
const PROVIDER_HISTORY_REJECTED_TOOL: &str = "tool_rejected";
const MAX_PLAN_STEPS: usize = 64;
const MAX_PLAN_STEP_CHARS: usize = 512;
const MAX_VERIFICATION_REQUIREMENTS: usize = 64;
const MAX_COMPACTION_PLAN_STEP_CHARS: usize = 160;
const REPEATED_FAILURE_RECOVERY_INSTRUCTIONS: &str = "The same repairable tool failure recurred. Read the registered tool schema and the previous tool result, then choose a different next action. Do not repeat the same call.";
const PLAN_COMPLETION_REQUIRED: &str = "Do not finalize yet. Complete every plan step, then call update_plan with all steps marked completed before providing the final answer.";
const EXACT_VERIFICATION_REQUIRED: &str =
    "completion gate rejected final answer: required verification commands are incomplete";

/// 一次 `AgentLoop` 运行的外部可观察生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    CancelRequested,
    Completed,
    Blocked,
    Cancelled,
    Failed,
}

impl AgentStatus {
    /// 返回稳定的生命周期状态字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl From<&str> for AgentStatus {
    fn from(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "cancel_requested" => Self::CancelRequested,
            "completed" => Self::Completed,
            "blocked" => Self::Blocked,
            "cancelled" | "canceled" => Self::Cancelled,
            "failed" | "max_turns_exceeded" => Self::Failed,
            _ => Self::Failed,
        }
    }
}

/// 工作区变更或必要检查后由完成门禁收集的证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct AgentVerification {
    pub required: bool,
    pub passed: bool,
    pub successful_command_count: u32,
    pub required_command_count: u32,
    pub satisfied_command_count: u32,
    pub unresolved_failures: Vec<String>,
}

/// 进入最终答复阶段前必须满足的一项精确命令范围要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentVerificationRequirement {
    pub command_scope_digest: String,
    pub required_success_count: u32,
}

impl AgentVerificationRequirement {
    /// 创建命令作用域验证要求。
    pub fn new(command_scope_digest: impl Into<String>, required_success_count: u32) -> Self {
        Self {
            command_scope_digest: command_scope_digest.into(),
            required_success_count,
        }
    }
}

/// 一个用户可见执行计划步骤的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// 一个计划步骤及其当前生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanStep {
    pub step: String,
    pub status: AgentPlanStepStatus,
}

/// 完整计划；校验确保其有界、唯一且无歧义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlan {
    pub steps: Vec<AgentPlanStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentPlanValidationFailure {
    Empty,
    TooManySteps,
    EmptyStep,
    StepTooLong,
    DuplicateStep,
    MultipleInProgress,
}

impl AgentPlanValidationFailure {
    fn code(self) -> &'static str {
        match self {
            Self::Empty => "plan_steps_empty",
            Self::TooManySteps => "plan_step_limit_exceeded",
            Self::EmptyStep => "plan_step_empty",
            Self::StepTooLong => "plan_step_too_long",
            Self::DuplicateStep => "plan_step_duplicate",
            Self::MultipleInProgress => "plan_multiple_in_progress",
        }
    }

    fn message(self) -> String {
        match self {
            Self::Empty => "plan must contain at least one step".to_string(),
            Self::TooManySteps => {
                format!("plan must not contain more than {MAX_PLAN_STEPS} steps")
            }
            Self::EmptyStep => "plan steps must not be empty".to_string(),
            Self::StepTooLong => {
                format!("plan steps must not exceed {MAX_PLAN_STEP_CHARS} characters")
            }
            Self::DuplicateStep => "plan steps must be unique".to_string(),
            Self::MultipleInProgress => "plan may have at most one in_progress step".to_string(),
        }
    }
}

impl AgentPlan {
    /// 校验计划步骤和完成状态契约。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_contract()
            .map_err(AgentPlanValidationFailure::message)
    }

    fn validate_contract(&self) -> Result<(), AgentPlanValidationFailure> {
        if self.steps.is_empty() {
            return Err(AgentPlanValidationFailure::Empty);
        }
        if self.steps.len() > MAX_PLAN_STEPS {
            return Err(AgentPlanValidationFailure::TooManySteps);
        }
        let mut unique_steps = BTreeSet::new();
        let mut in_progress_count = 0usize;
        for plan_step in &self.steps {
            let normalized_step = plan_step.step.trim();
            if normalized_step.is_empty() {
                return Err(AgentPlanValidationFailure::EmptyStep);
            }
            if normalized_step.chars().count() > MAX_PLAN_STEP_CHARS {
                return Err(AgentPlanValidationFailure::StepTooLong);
            }
            if !unique_steps.insert(normalized_step.to_string()) {
                return Err(AgentPlanValidationFailure::DuplicateStep);
            }
            if plan_step.status == AgentPlanStepStatus::InProgress {
                in_progress_count += 1;
            }
        }
        if in_progress_count > 1 {
            return Err(AgentPlanValidationFailure::MultipleInProgress);
        }
        Ok(())
    }

    /// 判断计划是否已完成。
    pub fn is_completed(&self) -> bool {
        self.steps
            .iter()
            .all(|plan_step| plan_step.status == AgentPlanStepStatus::Completed)
    }
}

/// 返回用于更新计划的独占控制 tool。
pub fn agent_control_tool_specs() -> Vec<ToolSpec> {
    vec![ToolSpec::new(
        UPDATE_PLAN_TOOL,
        "Replace the current execution plan with unique non-empty steps and at most one in_progress step",
        json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "The complete plan. Step text must be unique and at most one step may be in_progress.",
                    "minItems": 1,
                    "maxItems": MAX_PLAN_STEPS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {
                                "type": "string",
                                "description": "A unique, non-empty description of one execution step.",
                                "minLength": 1,
                                "maxLength": MAX_PLAN_STEP_CHARS
                            },
                            "status": {
                                "type": "string",
                                "description": "Use in_progress for at most one step.",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["steps"],
            "additionalProperties": false
        }),
        ToolExecutionMode::Exclusive,
        validate_plan_tool_input_contract,
    )]
}

/// 用于替换当前执行计划的模型输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanUpdateInput {
    pub steps: Vec<AgentPlanStep>,
}

impl AgentPlanUpdateInput {
    /// 将模型输入转换为已校验计划。
    pub fn into_plan(self) -> Result<AgentPlan, String> {
        let plan = AgentPlan { steps: self.steps };
        plan.validate()?;
        Ok(plan)
    }
}

/// 无效调用、修复尝试和被拒绝完成尝试的计数器。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRecoveryMetrics {
    pub invalid_tool_call_count: u32,
    pub repeated_tool_call_count: u32,
    pub repair_attempt_count: u32,
    pub completion_rejection_count: u32,
}

/// 从 loop 派生的公开运行状态，包括门禁证据和安全诊断信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunStatus {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub audit_events: Vec<Value>,
    pub verification: AgentVerification,
    pub plan: Option<AgentPlan>,
    pub plan_update_count: u32,
    pub recovery_metrics: AgentRecoveryMetrics,
    pub model_usage: ModelUsage,
    pub provider_attempts: ProviderAttemptMetadata,
    pub error: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub model_turn_limit: u32,
    #[serde(skip)]
    #[schemars(skip)]
    pub context_trace: Option<AgentContextTrace>,
    #[serde(skip)]
    #[schemars(skip)]
    pub error_category: Option<ModelErrorCategory>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_diagnostic: Option<ProviderDiagnostic>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_protocol_contract: Option<ProviderProtocolContract>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_capability_metadata: Option<ProviderCapabilityMetadata>,
}

impl AgentRunStatus {
    /// 构造普通失败状态。
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentStatus::Failed,
            completed: false,
            final_answer: None,
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            audit_events: Vec::new(),
            verification: AgentVerification::default(),
            plan: None,
            plan_update_count: 0,
            recovery_metrics: AgentRecoveryMetrics::default(),
            model_usage: ModelUsage::default(),
            provider_attempts: ProviderAttemptMetadata::default(),
            error: Some(message.into()),
            model_turn_limit: 0,
            context_trace: None,
            error_category: None,
            provider_diagnostic: None,
            provider_protocol_contract: None,
            provider_capability_metadata: None,
        }
    }

    /// 构造带稳定错误分类的失败状态。
    pub fn failed_with_category(
        message: impl Into<String>,
        error_category: Option<ModelErrorCategory>,
    ) -> Self {
        let mut status = Self::failed(message);
        status.error_category = error_category;
        status
    }

    /// 更新状态并保留已有诊断字段。
    pub fn with_status(mut self, status: AgentStatus) -> Self {
        self.completed = status == AgentStatus::Completed;
        self.status = status;
        self
    }
}

/// `AgentLoop` 所需执行 backend 的可用性和阻塞信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopCapability {
    pub available: bool,
    pub status: AgentStatus,
    pub reason: String,
    pub blockers: Vec<String>,
}

impl AgentLoopCapability {
    /// 构造可用能力摘要。
    pub fn available(reason: impl Into<String>) -> Self {
        Self {
            available: true,
            status: AgentStatus::Completed,
            reason: reason.into(),
            blockers: Vec::new(),
        }
    }

    /// 构造不可用能力摘要及 blocker。
    pub fn unavailable(reason: impl Into<String>, blocker: impl Into<String>) -> Self {
        Self {
            available: false,
            status: AgentStatus::Blocked,
            reason: reason.into(),
            blockers: vec![blocker.into()],
        }
    }
}

/// 一次运行的输入，包括当前 turn 上下文、安全历史、授权和校验规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopInput {
    pub thread_id: String,
    pub turn_id: String,
    pub model_preferences: ModelPreferences,
    #[serde(skip)]
    #[schemars(skip)]
    pub project_instructions: Option<String>,
    pub input: Vec<AgentContextItem>,
    pub interrupted: bool,
    pub max_turns: u32,
    pub approval_grants: Vec<ApprovalGrant>,
    #[serde(skip)]
    #[schemars(skip)]
    pub verification_requirements: Vec<AgentVerificationRequirement>,
}

impl AgentLoopInput {
    /// 创建最小 AgentLoop 输入。
    pub fn new(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        goal: impl Into<String>,
    ) -> Self {
        let turn_id = turn_id.into();
        Self {
            thread_id: thread_id.into(),
            turn_id,
            model_preferences: ModelPreferences::default(),
            project_instructions: None,
            input: vec![AgentContextItem::user("input_1", goal.into())],
            interrupted: false,
            max_turns: DEFAULT_MAX_AGENT_LOOP_TURNS,
            approval_grants: Vec::new(),
            verification_requirements: Vec::new(),
        }
    }

    /// 设置本轮模型名称。
    pub fn with_model_name(mut self, model_name: Option<String>) -> Self {
        self.model_preferences.model_name = model_name;
        self
    }

    /// 设置最大 model turn 数。
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// 设置完成前必须满足的验证要求。
    pub fn with_verification_requirements(
        mut self,
        requirements: impl IntoIterator<Item = AgentVerificationRequirement>,
    ) -> Self {
        self.verification_requirements = requirements.into_iter().collect();
        self
    }

    /// 附加项目指令文本。
    pub fn with_project_instructions(mut self, instructions: impl Into<String>) -> Self {
        let instructions = instructions.into();
        self.project_instructions = (!instructions.trim().is_empty()).then_some(instructions);
        self
    }

    /// 设置安全的历史上下文。
    pub fn with_history(mut self, history: impl IntoIterator<Item = AgentContextItem>) -> Self {
        let mut history: Vec<AgentContextItem> = history
            .into_iter()
            .filter_map(AgentContextItem::into_safe_history)
            .collect();
        history.extend(self.input);
        self.input = history;
        self
    }

    /// 设置 approval 恢复授权。
    pub fn with_approval_grant(mut self, grant: ApprovalGrant) -> Self {
        self.approval_grants.push(grant);
        self
    }
}

/// 已获批准的 tool call，并绑定到其请求、tool 和资源集合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalGrant {
    pub request_id: String,
    pub tool_name: String,
    pub resources: Vec<String>,
    pub outcome: ApprovalOutcome,
}

impl ApprovalGrant {
    /// 构造允许恢复的 approval 授权。
    pub fn allow<I, S>(
        request_id: impl Into<String>,
        tool_name: impl Into<String>,
        resources: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            request_id: request_id.into(),
            tool_name: tool_name.into(),
            resources: resources.into_iter().map(Into::into).collect(),
            outcome: ApprovalOutcome::Allow,
        }
    }
}

/// 一次运行的完整结果，包括待处理 approval 检查点和 tool 结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopResult {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub approval_requests: Vec<ApprovalRequest>,
    #[serde(skip)]
    #[schemars(skip)]
    pub pending_tool_calls: Vec<PendingToolCall>,
    #[serde(skip)]
    #[schemars(skip)]
    pub approval_checkpoints: Vec<Value>,
    pub tool_results: Vec<ToolResult>,
    pub verification: AgentVerification,
    pub plan: Option<AgentPlan>,
    pub plan_update_count: u32,
    pub recovery_metrics: AgentRecoveryMetrics,
    pub model_usage: ModelUsage,
    pub provider_attempts: ProviderAttemptMetadata,
    pub error: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub model_turn_limit: u32,
    #[serde(skip)]
    #[schemars(skip)]
    pub context_trace: Option<AgentContextTrace>,
    #[serde(skip)]
    #[schemars(skip)]
    pub error_category: Option<ModelErrorCategory>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_diagnostic: Option<ProviderDiagnostic>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_protocol_contract: Option<ProviderProtocolContract>,
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_capability_metadata: Option<ProviderCapabilityMetadata>,
}

impl AgentLoopResult {
    /// 读取指定 approval request 的 checkpoint。
    pub fn approval_checkpoint(&self, request_id: &str) -> Option<Value> {
        self.approval_checkpoints
            .iter()
            .find(|checkpoint| {
                checkpoint
                    .get("request_id")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value == request_id)
            })
            .cloned()
    }

    /// 将内部结果投影为持久化运行状态。
    pub fn to_run_status(&self) -> AgentRunStatus {
        AgentRunStatus {
            status: self.status.clone(),
            completed: self.completed,
            final_answer: self.final_answer.clone(),
            model_turns: self.model_turns,
            tool_calls: self.tool_calls,
            approval_count: self.approval_count,
            audit_events: audit_events_from_tool_results(&self.tool_results),
            verification: self.verification.clone(),
            plan: self.plan.clone(),
            plan_update_count: self.plan_update_count,
            recovery_metrics: self.recovery_metrics.clone(),
            model_usage: self.model_usage.clone(),
            provider_attempts: self.provider_attempts.clone(),
            error: self.error.clone(),
            model_turn_limit: self.model_turn_limit,
            context_trace: self.context_trace.clone(),
            error_category: self.error_category.clone(),
            provider_diagnostic: self.provider_diagnostic.clone(),
            provider_protocol_contract: self.provider_protocol_contract.clone(),
            provider_capability_metadata: self.provider_capability_metadata.clone(),
        }
    }
}

/// approval 暂停运行期间保留的规范化可执行 tool call 数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PendingToolCall {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
    pub resources: Vec<String>,
}

impl PendingToolCall {
    /// 从模型调用创建待执行记录。
    pub fn new(input: &AgentLoopInput, call: &ModelToolCall) -> Self {
        Self::new_with_profile(input, call, &PermissionProfile::workspace_write("."))
    }

    fn new_with_profile(
        input: &AgentLoopInput,
        call: &ModelToolCall,
        profile: &PermissionProfile,
    ) -> Self {
        Self {
            request_id: approval_request_id(input, call),
            tool_call_id: call.tool_call_id.clone(),
            tool_name: call.tool_name.clone(),
            raw_arguments: call.raw_arguments.clone(),
            resources: permission_resources_for_tool(call, profile),
        }
    }

    fn to_model_tool_call(&self) -> Result<ModelToolCall, serde_json::Error> {
        let arguments: Value = serde_json::from_str(&self.raw_arguments)?;
        Ok(ModelToolCall {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            raw_arguments: self.raw_arguments.clone(),
            arguments,
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        })
    }
}

/// 由 tool 结果和 approval 检查点共享的完成门禁状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct CompletionTracker {
    workspace_mutated: bool,
    verified_after_last_mutation: bool,
    successful_command_count: u32,
    #[serde(default)]
    required_command_counts: BTreeMap<String, u32>,
    #[serde(default)]
    satisfied_command_counts: BTreeMap<String, u32>,
    unresolved_failures: BTreeSet<String>,
}

impl CompletionTracker {
    fn from_requirements(requirements: &[AgentVerificationRequirement]) -> Result<Self, String> {
        if requirements.len() > MAX_VERIFICATION_REQUIREMENTS {
            return Err(format!(
                "verification requirements must not contain more than {MAX_VERIFICATION_REQUIREMENTS} entries"
            ));
        }
        let mut required_command_counts = BTreeMap::new();
        for requirement in requirements {
            if !is_sha256_fingerprint(&requirement.command_scope_digest) {
                return Err("verification requirement command digest is invalid".to_string());
            }
            if requirement.required_success_count == 0 {
                return Err(
                    "verification requirement success count must be greater than zero".to_string(),
                );
            }
            let count = required_command_counts
                .entry(requirement.command_scope_digest.clone())
                .or_insert(0u32);
            *count = count
                .checked_add(requirement.required_success_count)
                .ok_or_else(|| {
                    "verification requirement success count exceeds the supported range".to_string()
                })?;
        }
        Ok(Self {
            required_command_counts,
            ..Self::default()
        })
    }

    fn observe(&mut self, tool_result: &ToolResult) {
        let failure_group = match tool_result.failure_kind.as_ref() {
            Some(ToolFailureKind::Visibility) => TOOL_SELECTION_FAILURE_GROUP,
            _ => match tool_result.tool_name.as_str() {
                TOOL_EDIT | TOOL_PATCH => "workspace_mutation",
                TOOL_COMMAND => "verification",
                tool_name => tool_name,
            },
        };
        if tool_result.ok {
            self.unresolved_failures.retain(|failure| {
                !failure.starts_with(failure_group)
                    && !failure.starts_with(TOOL_SELECTION_FAILURE_PREFIX)
            });
            if matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH) {
                self.workspace_mutated = true;
                self.verified_after_last_mutation = false;
                self.satisfied_command_counts.clear();
            } else if tool_result.tool_name == TOOL_COMMAND {
                self.successful_command_count = self.successful_command_count.saturating_add(1);
                let scope_digest = successful_command_scope_digest(tool_result);
                if self.required_command_counts.is_empty() && self.workspace_mutated {
                    self.verified_after_last_mutation = scope_digest.is_some();
                } else if let Some(result_id) = scope_digest
                    && let Some(required_count) = self.required_command_counts.get(result_id)
                {
                    let satisfied_count = self
                        .satisfied_command_counts
                        .entry(result_id.to_string())
                        .or_insert(0);
                    if *satisfied_count < *required_count {
                        *satisfied_count = satisfied_count.saturating_add(1);
                    }
                }
            }
        } else if is_repairable_tool_result(tool_result) {
            let error_code = tool_result
                .error_code
                .as_deref()
                .unwrap_or("tool_execution_failed");
            self.unresolved_failures
                .insert(format!("{failure_group}:{error_code}"));
        }
    }

    fn allows_final(&self) -> bool {
        self.unresolved_failures.is_empty() && self.verification_satisfied()
    }

    fn rejection_reason(&self) -> String {
        if !self.unresolved_failures.is_empty() {
            return format!(
                "completion gate rejected final answer: unresolved failures: {}",
                self.unresolved_failures
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !self.required_command_counts.is_empty() {
            return EXACT_VERIFICATION_REQUIRED.to_string();
        }
        POST_MUTATION_VERIFICATION_REQUIRED.to_string()
    }

    fn feedback(&self) -> String {
        if !self.unresolved_failures.is_empty() {
            return format!(
                "Do not finalize yet. Resolve these failures and rerun the relevant verification: {}.",
                self.unresolved_failures
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !self.required_command_counts.is_empty() {
            return format!(
                "Do not finalize yet. Run every exact verification command required by the task after the latest workspace mutation. {} of {} required successful command results are currently satisfied.",
                self.satisfied_command_count(),
                self.required_command_count()
            );
        }
        "Do not finalize yet. Run a relevant verification command after the latest workspace mutation, inspect its result, and only then provide the final answer."
            .to_string()
    }

    fn summary(&self) -> AgentVerification {
        let required_command_count = self.required_command_count();
        let satisfied_command_count = self.satisfied_command_count();
        let required = self.workspace_mutated || required_command_count > 0;
        AgentVerification {
            required,
            passed: required && self.allows_final(),
            successful_command_count: self.successful_command_count,
            required_command_count: if required_command_count > 0 {
                required_command_count
            } else {
                u32::from(self.workspace_mutated)
            },
            satisfied_command_count: if required_command_count > 0 {
                satisfied_command_count
            } else {
                u32::from(self.workspace_mutated && self.verified_after_last_mutation)
            },
            unresolved_failures: self.unresolved_failures.iter().cloned().collect(),
        }
    }

    fn verification_satisfied(&self) -> bool {
        if self.required_command_counts.is_empty() {
            return !self.workspace_mutated || self.verified_after_last_mutation;
        }
        self.required_command_counts
            .iter()
            .all(|(digest, required)| {
                self.satisfied_command_counts
                    .get(digest)
                    .copied()
                    .unwrap_or(0)
                    >= *required
            })
    }

    fn required_command_count(&self) -> u32 {
        self.required_command_counts
            .values()
            .copied()
            .fold(0u32, u32::saturating_add)
    }

    fn satisfied_command_count(&self) -> u32 {
        self.required_command_counts
            .iter()
            .map(|(digest, required)| {
                self.satisfied_command_counts
                    .get(digest)
                    .copied()
                    .unwrap_or(0)
                    .min(*required)
            })
            .fold(0u32, u32::saturating_add)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RepairFailureState {
    signature: String,
    consecutive_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointToolResult {
    result: ToolResult,
    #[serde(default)]
    result_id: Option<String>,
    audit_metadata: Option<Value>,
}

impl CheckpointToolResult {
    fn from_tool_result(result: &ToolResult) -> Self {
        Self {
            result: result.clone(),
            result_id: result.result_id.clone(),
            audit_metadata: result.audit_metadata().cloned(),
        }
    }

    fn into_tool_result(self) -> ToolResult {
        let mut result = self.result;
        result.result_id = self.result_id;
        match self.audit_metadata {
            Some(audit_metadata) => result.with_audit(audit_metadata),
            None => result,
        }
    }
}

/// 用于安全恢复受 approval 控制的 tool call 的可序列化暂停状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentLoopCheckpoint {
    #[serde(flatten)]
    pending_tool_call: PendingToolCall,
    checkpoint_version: u32,
    thread_id: String,
    turn_id: String,
    messages: Vec<ModelMessage>,
    tool_results: Vec<CheckpointToolResult>,
    used_approval_grants: Vec<String>,
    approval_count: u32,
    model_turns: u32,
    completion: CompletionTracker,
    last_completion_error: Option<String>,
    #[serde(default)]
    plan: Option<AgentPlan>,
    #[serde(default)]
    plan_update_count: u32,
    #[serde(default)]
    recovery_metrics: AgentRecoveryMetrics,
    #[serde(default)]
    model_usage: ModelUsage,
    #[serde(default)]
    provider_attempts: ProviderAttemptMetadata,
    #[serde(default)]
    context_trace: Option<AgentContextTrace>,
    #[serde(default)]
    seen_tool_call_fingerprints: Vec<String>,
    #[serde(default)]
    last_repair_failure: Option<RepairFailureState>,
}

/// 在形成 `AgentLoopResult` 前跨模型提供方 turn 累积的可变状态。
struct AgentLoopState {
    messages: Vec<ModelMessage>,
    tool_results: Vec<ToolResult>,
    approval_requests: Vec<ApprovalRequest>,
    pending_tool_calls: Vec<PendingToolCall>,
    approval_checkpoints: Vec<Value>,
    used_approval_grants: BTreeSet<String>,
    prior_approval_count: u32,
    completion: CompletionTracker,
    last_completion_error: Option<String>,
    plan: Option<AgentPlan>,
    plan_update_count: u32,
    recovery_metrics: AgentRecoveryMetrics,
    model_usage: ModelUsage,
    provider_attempts: ProviderAttemptMetadata,
    seen_tool_call_fingerprints: BTreeSet<String>,
    last_repair_failure: Option<RepairFailureState>,
    model_turn_limit: u32,
    context_trace: Option<AgentContextTrace>,
    provider_protocol_contract: Option<ProviderProtocolContract>,
    provider_capability_metadata: Option<ProviderCapabilityMetadata>,
}

impl AgentLoopState {
    fn new(
        messages: Vec<ModelMessage>,
        model_turn_limit: u32,
        context_trace: Option<AgentContextTrace>,
    ) -> Self {
        Self {
            messages,
            tool_results: Vec::new(),
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            approval_checkpoints: Vec::new(),
            used_approval_grants: BTreeSet::new(),
            prior_approval_count: 0,
            completion: CompletionTracker::default(),
            last_completion_error: None,
            plan: None,
            plan_update_count: 0,
            recovery_metrics: AgentRecoveryMetrics::default(),
            model_usage: ModelUsage::default(),
            provider_attempts: ProviderAttemptMetadata::default(),
            seen_tool_call_fingerprints: BTreeSet::new(),
            last_repair_failure: None,
            model_turn_limit,
            context_trace,
            provider_protocol_contract: None,
            provider_capability_metadata: None,
        }
    }

    fn finish(
        self,
        status: AgentStatus,
        completed: bool,
        final_answer: Option<String>,
        model_turns: u32,
        error: Option<String>,
    ) -> AgentLoopResult {
        self.finish_with_model_error(status, completed, final_answer, model_turns, error, None)
    }

    fn finish_with_model_error(
        self,
        status: AgentStatus,
        completed: bool,
        final_answer: Option<String>,
        model_turns: u32,
        error: Option<String>,
        model_error: Option<&ModelError>,
    ) -> AgentLoopResult {
        let approval_count = self
            .prior_approval_count
            .saturating_add(self.approval_requests.len() as u32);
        let public_plan = self.plan.as_ref().map(safe_agent_plan);
        AgentLoopResult {
            status,
            completed,
            final_answer,
            model_turns,
            tool_calls: self.tool_results.len() as u32,
            approval_count,
            approval_requests: self.approval_requests,
            pending_tool_calls: self.pending_tool_calls,
            approval_checkpoints: self.approval_checkpoints,
            tool_results: self.tool_results,
            verification: self.completion.summary(),
            plan: public_plan,
            plan_update_count: self.plan_update_count,
            recovery_metrics: self.recovery_metrics,
            model_usage: self.model_usage,
            provider_attempts: self.provider_attempts,
            error,
            model_turn_limit: self.model_turn_limit,
            context_trace: self.context_trace,
            error_category: model_error.map(ModelError::category),
            provider_diagnostic: model_error.map(ModelError::provider_diagnostic),
            provider_protocol_contract: self.provider_protocol_contract,
            provider_capability_metadata: self.provider_capability_metadata,
        }
    }

    fn record_provider_negotiation(
        &mut self,
        contract: &ProviderProtocolContract,
        metadata: &ProviderCapabilityMetadata,
    ) {
        self.provider_protocol_contract = Some(contract.clone());
        self.provider_capability_metadata = Some(metadata.clone());
    }

    fn record_provider_negotiation_error(&mut self, error: &ProviderError) {
        self.provider_protocol_contract = None;
        self.provider_capability_metadata = error.capability_metadata.as_deref().cloned();
    }

    fn approval_count(&self) -> u32 {
        self.prior_approval_count
            .saturating_add(self.approval_requests.len() as u32)
    }

    fn observe_model_response(&mut self, response: &ModelTurnResponse) {
        self.model_usage.input_tokens = self
            .model_usage
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        self.model_usage.output_tokens = self
            .model_usage
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        self.model_usage.total_tokens = self
            .model_usage
            .total_tokens
            .saturating_add(response.usage.total_tokens);
        self.model_usage.cached_input_tokens = self
            .model_usage
            .cached_input_tokens
            .saturating_add(response.usage.cached_input_tokens);
        self.model_usage.reasoning_tokens = self
            .model_usage
            .reasoning_tokens
            .saturating_add(response.usage.reasoning_tokens);
        if let Some(cost) = response.usage.cost_estimate {
            self.model_usage.cost_estimate =
                Some(self.model_usage.cost_estimate.unwrap_or_default().max(0.0) + cost.max(0.0));
        }
        if let Some(metadata) = &response.provider_attempt_metadata {
            self.provider_attempts.attempt_count = self
                .provider_attempts
                .attempt_count
                .saturating_add(metadata.attempt_count);
            self.provider_attempts.retry_count = self
                .provider_attempts
                .retry_count
                .saturating_add(metadata.retry_count);
            self.provider_attempts.latency_ms = self
                .provider_attempts
                .latency_ms
                .saturating_add(metadata.latency_ms);
        }
    }

    fn checkpoint(
        &self,
        input: &AgentLoopInput,
        pending_tool_call: &PendingToolCall,
        model_turns: u32,
    ) -> Value {
        let checkpoint = AgentLoopCheckpoint {
            pending_tool_call: pending_tool_call.clone(),
            checkpoint_version: APPROVAL_CHECKPOINT_VERSION,
            thread_id: input.thread_id.clone(),
            turn_id: input.turn_id.clone(),
            messages: self.messages.clone(),
            tool_results: self
                .tool_results
                .iter()
                .map(CheckpointToolResult::from_tool_result)
                .collect(),
            used_approval_grants: self.used_approval_grants.iter().cloned().collect(),
            approval_count: self.approval_count(),
            model_turns,
            completion: self.completion.clone(),
            last_completion_error: self.last_completion_error.clone(),
            plan: self.plan.clone(),
            plan_update_count: self.plan_update_count,
            recovery_metrics: self.recovery_metrics.clone(),
            model_usage: self.model_usage.clone(),
            provider_attempts: self.provider_attempts.clone(),
            context_trace: self.context_trace.clone(),
            seen_tool_call_fingerprints: self.seen_tool_call_fingerprints.iter().cloned().collect(),
            last_repair_failure: self.last_repair_failure.clone(),
        };
        serde_json::to_value(checkpoint).expect("AgentLoop checkpoint serializes")
    }

    fn allows_final(&self) -> bool {
        self.completion.allows_final() && self.plan.as_ref().is_none_or(AgentPlan::is_completed)
    }

    fn finalization_ready(&self) -> bool {
        // `allows_final` can be true before the first model turn for simple read-only work.
        // Passed required verification supplies the additional evidence that the next request is
        // only collecting the final answer. An explicit plan, when present, must also be complete.
        self.completion.summary().passed && self.plan.as_ref().is_none_or(AgentPlan::is_completed)
    }

    fn completion_rejection_reason(&self) -> String {
        let mut reasons = Vec::new();
        if self.plan.as_ref().is_some_and(|plan| !plan.is_completed()) {
            reasons.push(
                "completion gate rejected final answer: plan has incomplete steps".to_string(),
            );
        }
        if !self.completion.allows_final() {
            reasons.push(self.completion.rejection_reason());
        }
        reasons.join("; ")
    }

    fn completion_feedback(&self) -> String {
        let mut feedback = Vec::new();
        if self.plan.as_ref().is_some_and(|plan| !plan.is_completed()) {
            feedback.push(PLAN_COMPLETION_REQUIRED.to_string());
        }
        if !self.completion.allows_final() {
            feedback.push(self.completion.feedback());
        }
        feedback.join(" ")
    }

    fn observe_model_tool_call(
        &mut self,
        call: &ModelToolCall,
        allowed_tool_names: &[String],
    ) -> (String, bool) {
        let fingerprint = tool_call_fingerprint(call);
        if !self.seen_tool_call_fingerprints.insert(fingerprint.clone()) {
            self.recovery_metrics.repeated_tool_call_count = self
                .recovery_metrics
                .repeated_tool_call_count
                .saturating_add(1);
        }
        let invalid = call.parse_status != ModelToolParseStatus::Valid
            || !call.arguments.is_object()
            || call.tool_name.trim().is_empty()
            || !allowed_tool_names
                .iter()
                .any(|tool_name| tool_name == &call.tool_name);
        if invalid {
            self.recovery_metrics.invalid_tool_call_count = self
                .recovery_metrics
                .invalid_tool_call_count
                .saturating_add(1);
        }
        (fingerprint, invalid)
    }

    fn observe_tool_result(
        &mut self,
        tool_result: &ToolResult,
        tool_call_fingerprint: &str,
    ) -> Option<String> {
        self.completion.observe(tool_result);
        if tool_result.ok {
            self.last_repair_failure = None;
            return None;
        }
        if !is_repairable_tool_result(tool_result) {
            self.last_repair_failure = None;
            return None;
        }
        self.recovery_metrics.repair_attempt_count =
            self.recovery_metrics.repair_attempt_count.saturating_add(1);
        let error_code = tool_result
            .error_code
            .as_deref()
            .unwrap_or("tool_execution_failed");
        let signature = repair_failure_signature(tool_call_fingerprint, error_code);
        let consecutive_count = if self
            .last_repair_failure
            .as_ref()
            .is_some_and(|failure| failure.signature == signature)
        {
            self.last_repair_failure
                .as_ref()
                .map_or(1, |failure| failure.consecutive_count.saturating_add(1))
        } else {
            1
        };
        self.last_repair_failure = Some(RepairFailureState {
            signature,
            consecutive_count,
        });
        (consecutive_count >= 2).then_some(REPEATED_FAILURE_RECOVERY_INSTRUCTIONS.to_string())
    }
}

#[derive(Clone)]
struct PreparedToolCall {
    call: ModelToolCall,
    fingerprint: String,
    execution_mode: Option<ToolExecutionMode>,
    decision: Option<ToolBrokerDecision>,
    rejection: Option<ToolResult>,
}

enum ToolBatchControl {
    Continue,
    Blocked,
    Failed(String),
    Cancelled,
}

/// 编排模型提供方 turn、策略决策、沙箱 tool、approval 和最终答复阶段。
pub struct AgentLoop<P> {
    provider: P,
    tool_broker: ToolBroker,
    policy: PolicyEngine,
    workspace_tools: Option<WorkspaceTools>,
    cancellation: CancellationToken,
}

impl<P> AgentLoop<P>
where
    P: Provider,
{
    /// 创建带 broker、policy 和 provider 的 AgentLoop。
    pub fn new(provider: P, tool_broker: ToolBroker, policy: PolicyEngine) -> Self {
        Self {
            provider,
            tool_broker,
            policy,
            workspace_tools: None,
            cancellation: CancellationToken::new(),
        }
    }

    /// 绑定工作区 tool 执行器。
    pub fn with_workspace_tools(mut self, workspace_tools: WorkspaceTools) -> Self {
        self.workspace_tools = Some(workspace_tools);
        self
    }

    /// 绑定取消 token。
    pub fn with_cancellation_token(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// 运行一个新 turn，直到完成、因 approval 阻塞、被取消或拒绝继续执行。
    pub fn run(&self, input: &AgentLoopInput) -> AgentLoopResult {
        let mut state = AgentLoopState::new(Vec::new(), input.max_turns.max(1), None);
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, 0, None);
        }
        let completion =
            match CompletionTracker::from_requirements(&input.verification_requirements) {
                Ok(completion) => completion,
                Err(error) => {
                    return state.finish(AgentStatus::Failed, false, None, 0, Some(error));
                }
            };
        state.completion = completion;
        let (capabilities, mut state) = match self.negotiate_tool_capabilities(input, state, 0) {
            ControlFlow::Continue(result) => result,
            ControlFlow::Break(result) => return result,
        };
        let max_tool_calls = effective_max_tool_calls(&capabilities);
        let budget = match context_budget(input, &self.tool_broker, &capabilities, max_tool_calls) {
            Ok(budget) => budget,
            Err(error) => {
                return state.finish(AgentStatus::Failed, false, None, 0, Some(error));
            }
        };
        let context = assemble_context_items_with_budget(&input.input, &budget);
        if current_turn_excluded(input, &context) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                0,
                Some(CURRENT_TURN_CONTEXT_OVERFLOW_ERROR.to_string()),
            );
        }
        state.messages = model_messages_from_input(input, &context, max_tool_calls);
        state.context_trace = Some(AgentContextTrace::from(&context));
        self.continue_run(input, &budget, &capabilities, max_tool_calls, state, 0)
    }

    /// 在每次模型提供方响应或 tool 结果后推进状态机。
    fn continue_run(
        &self,
        input: &AgentLoopInput,
        budget: &ContextBudget,
        capabilities: &ProviderProtocolContract,
        max_tool_calls: u32,
        mut state: AgentLoopState,
        model_turn_offset: u32,
    ) -> AgentLoopResult {
        let max_turns = input.max_turns.max(1);
        if model_turn_offset > max_turns
            || (model_turn_offset == max_turns && !state.finalization_ready())
        {
            let model_turns = model_turn_offset.max(max_turns);
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turns,
                Some("max turns exceeded".to_string()),
            );
        }
        let mut finalization_attempted = false;
        let mut actual_model_turns = model_turn_offset;
        // 包含端点只保留给终态；没有 readiness 时仍维持普通工作回合上限及其失败语义。
        for turn_index in model_turn_offset..=max_turns {
            let finalization_only = state.finalization_ready();
            if !finalization_only && turn_index == max_turns {
                break;
            }
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index, None);
            }
            if finalization_only {
                if finalization_attempted {
                    break;
                }
                finalization_attempted = true;
            }
            let tool_view = if finalization_only {
                ModelToolView::finalization()
            } else {
                match model_tool_view(&self.tool_broker, capabilities, max_tool_calls) {
                    Ok(tool_view) => tool_view,
                    Err(error) => {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            turn_index,
                            Some(error),
                        );
                    }
                }
            };
            let visible_tool_names = tool_view.visible_tool_names.clone();
            if !model_request_fits_context(&tool_view.tools, &state.messages, budget) {
                let Some(compaction) = compact_model_messages(&tool_view.tools, &state, budget)
                else {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        turn_index,
                        Some(MODEL_REQUEST_CONTEXT_OVERFLOW_ERROR.to_string()),
                    );
                };
                if let Some(context_trace) = &mut state.context_trace {
                    context_trace.record_compaction(&compaction);
                }
                state.messages = compaction.messages;
            }
            let request = model_turn_request(
                input,
                budget,
                turn_index,
                &state,
                tool_view,
                capabilities,
                finalization_only,
            );
            let request_validation =
                validate_model_request_with_capabilities(&request, Some(capabilities));
            if !request_validation.valid {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(format!(
                        "model request validation failed: {}",
                        request_validation.errors.join(", ")
                    )),
                );
            }
            let response = match self.provider.complete(&request, &self.cancellation) {
                Ok(response) => response,
                Err(error) => provider_error_response(&request, error),
            };
            state.observe_model_response(&response);
            actual_model_turns = turn_index.saturating_add(1);
            if self.is_cancelled(input) {
                return state.finish(
                    AgentStatus::Cancelled,
                    false,
                    None,
                    actual_model_turns,
                    None,
                );
            }
            if response.status != ModelTurnStatus::Success {
                let model_error = response.error.as_ref();
                return state.finish_with_model_error(
                    AgentStatus::Failed,
                    false,
                    None,
                    actual_model_turns,
                    model_error.map(|error| error.message.clone()),
                    model_error,
                );
            }
            let provider_tool_names = request
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>();
            let validation = validate_model_turn_response(
                &request,
                &response,
                &provider_tool_names,
                Some(capabilities),
            );
            if !validation.valid {
                for call in &response.tool_calls {
                    state.observe_model_tool_call(call, &provider_tool_names);
                }
                let model_error = model_response_validation_error(validation.errors);
                return state.finish_with_model_error(
                    AgentStatus::Failed,
                    false,
                    None,
                    actual_model_turns,
                    Some(model_error.message.clone()),
                    Some(&model_error),
                );
            }
            if response.assistant_message.as_ref().is_some_and(|message| {
                !message.tool_calls.is_empty() && message.tool_calls != response.tool_calls
            }) {
                let model_error = model_response_validation_error(vec![
                    "assistant_tool_calls_mismatch".to_string(),
                ]);
                return state.finish_with_model_error(
                    AgentStatus::Failed,
                    false,
                    None,
                    actual_model_turns,
                    Some(model_error.message.clone()),
                    Some(&model_error),
                );
            }
            if response.tool_calls.is_empty() {
                let final_answer = assistant_message_text(response.assistant_message.as_ref());
                if final_answer.trim().is_empty() {
                    state.recovery_metrics.completion_rejection_count = state
                        .recovery_metrics
                        .completion_rejection_count
                        .saturating_add(1);
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    );
                }
                if state.allows_final() {
                    return state.finish(
                        AgentStatus::Completed,
                        true,
                        Some(final_answer),
                        actual_model_turns,
                        None,
                    );
                }
                state.recovery_metrics.completion_rejection_count = state
                    .recovery_metrics
                    .completion_rejection_count
                    .saturating_add(1);
                state.last_completion_error = Some(state.completion_rejection_reason());
                state.messages.push(
                    response
                        .assistant_message
                        .unwrap_or_else(|| ModelMessage::text(ModelRole::Assistant, final_answer)),
                );
                state.messages.push(ModelMessage::text(
                    ModelRole::Developer,
                    state.completion_feedback(),
                ));
                continue;
            }
            let execution_tool_calls =
                resolve_model_tool_calls(&response.tool_calls, &visible_tool_names);
            let execution_tool_names = execution_tool_calls
                .iter()
                .map(|call| call.tool_name.clone())
                .collect::<Vec<_>>();
            let observed_tool_calls = execution_tool_calls
                .iter()
                .map(|call| state.observe_model_tool_call(call, &execution_tool_names))
                .collect::<Vec<_>>();
            let assistant_tool_message = provider_history_assistant_message(
                response.assistant_message.as_ref(),
                &response.tool_calls,
                &execution_tool_calls,
            );
            state.messages.push(assistant_tool_message);
            match self.process_tool_calls(
                input,
                &execution_tool_calls,
                &response.tool_calls,
                &observed_tool_calls,
                &mut state,
                actual_model_turns,
            ) {
                ToolBatchControl::Continue => {}
                ToolBatchControl::Blocked => {
                    return state.finish(
                        AgentStatus::Blocked,
                        false,
                        None,
                        actual_model_turns,
                        None,
                    );
                }
                ToolBatchControl::Cancelled => {
                    return state.finish(
                        AgentStatus::Cancelled,
                        false,
                        None,
                        actual_model_turns,
                        None,
                    );
                }
                ToolBatchControl::Failed(error) => {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(error),
                    );
                }
            }
        }
        let error = state
            .last_completion_error
            .take()
            .unwrap_or_else(|| "max turns exceeded".to_string());
        state.finish(
            AgentStatus::Failed,
            false,
            None,
            actual_model_turns,
            Some(error),
        )
    }

    /// 恢复已校验的 approval 检查点，执行已批准调用，并继续运行。
    pub fn resume_pending_tool_call(
        &self,
        input: &AgentLoopInput,
        pending: &PendingToolCall,
        checkpoint_payload: &Value,
    ) -> AgentLoopResult {
        if self.is_cancelled(input) {
            return AgentLoopState::new(Vec::new(), input.max_turns.max(1), None).finish(
                AgentStatus::Cancelled,
                false,
                None,
                0,
                None,
            );
        }
        if let Err(error) = CompletionTracker::from_requirements(&input.verification_requirements) {
            return failed_result(error);
        }
        let call = match pending
            .to_model_tool_call()
            .map_err(|error| format!("invalid pending tool call arguments: {error}"))
            .and_then(|call| {
                self.tool_broker
                    .validate_execution_input(&call.tool_name, &call.arguments)
                    .map_err(|error| format!("invalid pending execution input: {}", error.code))?;
                let call = bind_tool_call_to_profile(&call, &self.policy.profile)
                    .map_err(|error| error.to_string())?;
                self.tool_broker
                    .validate_execution_input(&call.tool_name, &call.arguments)
                    .map_err(|error| format!("invalid rebound execution input: {}", error.code))?;
                Ok(call)
            }) {
            Ok(call) => call,
            Err(error) => {
                return AgentLoopState::new(Vec::new(), input.max_turns.max(1), None).finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    0,
                    Some(error),
                );
            }
        };
        let (state, model_turn_offset) = match restore_checkpoint(
            input,
            pending,
            checkpoint_payload,
            &self.tool_broker,
            &self.policy.profile,
        ) {
            Ok(restored) => restored,
            Err(error) => {
                return AgentLoopState::new(Vec::new(), input.max_turns.max(1), None).finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    0,
                    Some(error),
                );
            }
        };
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, model_turn_offset, None);
        }
        let (capabilities, mut state) =
            match self.negotiate_tool_capabilities(input, state, model_turn_offset) {
                ControlFlow::Continue(result) => result,
                ControlFlow::Break(result) => return result,
            };
        let max_tool_calls = effective_max_tool_calls(&capabilities);
        let budget = match context_budget(input, &self.tool_broker, &capabilities, max_tool_calls) {
            Ok(budget) => budget,
            Err(error) => {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    model_turn_offset,
                    Some(error),
                );
            }
        };
        refresh_developer_instructions(&mut state.messages, input, max_tool_calls);
        let context = assemble_context_items_with_budget(&input.input, &budget);
        if let Some(context_trace) = &mut state.context_trace {
            context_trace.refresh_context(&context);
        } else {
            state.context_trace = Some(AgentContextTrace::from(&context));
        }
        if current_turn_excluded(input, &context) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(CURRENT_TURN_CONTEXT_OVERFLOW_ERROR.to_string()),
            );
        }
        let decision = self.tool_decision(input, &call, &mut state.used_approval_grants);
        if !matches!(decision, ToolBrokerDecision::Approved { .. }) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some("pending tool call approval did not match".to_string()),
            );
        }
        let tool_call_fingerprint = tool_call_fingerprint(&call);
        let tool_result = self.execute_tool(&call, decision, &mut state);
        let tool_result = if self.is_cancelled(input) && tool_result.ok {
            cancelled_tool_result(&call)
        } else {
            tool_result
        };
        let failed_tool_result = !tool_result.ok;
        let recovery_feedback = state.observe_tool_result(&tool_result, &tool_call_fingerprint);
        state.messages.push(tool_result_message(&tool_result, None));
        state.tool_results.push(tool_result.clone());
        if let Some(feedback) = recovery_feedback {
            state
                .messages
                .push(ModelMessage::text(ModelRole::Developer, feedback));
        }
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, model_turn_offset, None);
        }
        if failed_tool_result && !is_repairable_tool_result(&tool_result) {
            let error_code = tool_result
                .error_code
                .as_deref()
                .unwrap_or("tool_execution_failed");
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(format!("tool execution failed: {error_code}")),
            );
        }
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            model_turn_offset,
        )
    }

    /// 协商模型提供方能力并记录结果，然后构建模型上下文。
    fn negotiate_tool_capabilities(
        &self,
        input: &AgentLoopInput,
        mut state: AgentLoopState,
        model_turns: u32,
    ) -> ControlFlow<AgentLoopResult, (ProviderProtocolContract, AgentLoopState)> {
        if self.is_cancelled(input) {
            return ControlFlow::Break(state.finish(
                AgentStatus::Cancelled,
                false,
                None,
                model_turns,
                None,
            ));
        }
        match self
            .provider
            .negotiate_tool_capabilities(&input.model_preferences, &self.cancellation)
        {
            Ok(negotiation) => {
                state.record_provider_negotiation(&negotiation.contract, &negotiation.metadata);
                if self.is_cancelled(input) {
                    return ControlFlow::Break(state.finish(
                        AgentStatus::Cancelled,
                        false,
                        None,
                        model_turns,
                        None,
                    ));
                }
                ControlFlow::Continue((negotiation.contract, state))
            }
            Err(error) => {
                state.record_provider_negotiation_error(&error);
                if self.is_cancelled(input)
                    || error.error.category() == ModelErrorCategory::Cancelled
                {
                    return ControlFlow::Break(state.finish(
                        AgentStatus::Cancelled,
                        false,
                        None,
                        model_turns,
                        None,
                    ));
                }
                ControlFlow::Break(state.finish_with_model_error(
                    AgentStatus::Failed,
                    false,
                    None,
                    model_turns,
                    Some(error.message),
                    Some(&error.error),
                ))
            }
        }
    }

    fn is_cancelled(&self, input: &AgentLoopInput) -> bool {
        input.interrupted || self.cancellation.is_cancelled()
    }

    fn tool_decision(
        &self,
        input: &AgentLoopInput,
        call: &ModelToolCall,
        used_approval_grants: &mut BTreeSet<String>,
    ) -> ToolBrokerDecision {
        if call.parse_status != ModelToolParseStatus::Valid || !call.arguments.is_object() {
            return ToolBrokerDecision::deny_with_kind(
                ToolFailureKind::Input,
                "invalid tool call arguments",
            );
        }
        let request_id = approval_request_id(input, call);
        let resources = permission_resources_for_tool(call, &self.policy.profile);
        let permission = self.tool_permission_decision(call);
        if used_approval_grants.contains(&request_id) {
            return ToolBrokerDecision::deny_with_kind(
                ToolFailureKind::Approval,
                "approval grant already consumed",
            );
        }
        if !matches!(permission.outcome, PermissionDecisionOutcome::Deny)
            && let Some(grant) = input.approval_grants.iter().find(|grant| {
                grant.request_id == request_id
                    && grant.tool_name == call.tool_name
                    && grant.resources == resources
                    && matches!(grant.outcome, ApprovalOutcome::Allow)
            })
        {
            used_approval_grants.insert(grant.request_id.clone());
            return ToolBrokerDecision::approved(grant.request_id.clone());
        }
        match permission.outcome {
            PermissionDecisionOutcome::Allow => ToolBrokerDecision::Allow,
            PermissionDecisionOutcome::Deny => ToolBrokerDecision::deny_with_kind(
                permission_failure_kind(&permission.cause),
                permission.reason,
            ),
            PermissionDecisionOutcome::Ask => {
                ToolBrokerDecision::ask(request_id, permission.reason)
            }
        }
    }

    fn tool_permission_decision(&self, call: &ModelToolCall) -> PermissionDecision {
        let resources = permission_resources_for_tool(call, &self.policy.profile);
        let mut operations = vec![permission_operation_for_tool(&call.tool_name)];
        if call.tool_name == TOOL_COMMAND
            && self.policy.profile.network_access == NetworkAccess::Allowed
        {
            operations.push(PermissionOperation::Network);
        }
        let mut first_allow = None;
        let mut first_ask = None;
        for operation in operations {
            for resource in &resources {
                let mut request =
                    PermissionRequest::new(call.tool_name.clone(), operation, resource.clone());
                if is_protected_path(resource) {
                    request = request.with_sensitive_resource();
                }
                let decision = self.policy.evaluate(&request);
                match decision.outcome {
                    PermissionDecisionOutcome::Deny => return decision,
                    PermissionDecisionOutcome::Ask if first_ask.is_none() => {
                        first_ask = Some(decision);
                    }
                    PermissionDecisionOutcome::Allow if first_allow.is_none() => {
                        first_allow = Some(decision);
                    }
                    _ => {}
                }
            }
        }
        first_ask.or(first_allow).unwrap_or_else(|| {
            PermissionDecision::new(PermissionDecisionOutcome::Ask, "no tool resource")
        })
    }

    /// 对模型提供方的 tool 批次执行预检、授权、执行或创建检查点。
    fn process_tool_calls(
        &self,
        input: &AgentLoopInput,
        calls: &[ModelToolCall],
        model_visible_calls: &[ModelToolCall],
        observed: &[(String, bool)],
        state: &mut AgentLoopState,
        next_model_turn: u32,
    ) -> ToolBatchControl {
        if self.is_cancelled(input) {
            return ToolBatchControl::Cancelled;
        }
        debug_assert_eq!(calls.len(), model_visible_calls.len());
        debug_assert_eq!(calls.len(), observed.len());
        let mut prepared = calls
            .iter()
            .zip(model_visible_calls)
            .zip(observed)
            .map(
                |((call, model_visible_call), (fingerprint, invalid_was_observed))| {
                    debug_assert_eq!(call.tool_call_id, model_visible_call.tool_call_id);
                    self.prepare_tool_call(call, fingerprint, *invalid_was_observed, state)
                },
            )
            .collect::<Vec<_>>();
        if self.is_cancelled(input) {
            return ToolBatchControl::Cancelled;
        }

        let mut staged_approval_grants = state.used_approval_grants.clone();
        if !prepared.iter().any(|call| call.rejection.is_some()) {
            for prepared_call in &mut prepared {
                let decision =
                    self.tool_decision(input, &prepared_call.call, &mut staged_approval_grants);
                prepared_call.rejection = matches!(decision, ToolBrokerDecision::Deny { .. })
                    .then(|| self.decision_result(&prepared_call.call, &decision));
                prepared_call.decision = Some(decision);
            }
        }

        if prepared.len() > 1
            && prepared.iter().any(|call| {
                call.rejection.is_some()
                    || call.execution_mode == Some(ToolExecutionMode::Exclusive)
                    || matches!(call.decision, Some(ToolBrokerDecision::Ask { .. }))
            })
        {
            let results = prepared
                .drain(..)
                .map(|call| {
                    let result = self.batch_rejection_result(&call);
                    (call, result)
                })
                .collect::<Vec<_>>();
            return self.record_tool_results(input, state, results, true);
        }

        if prepared.len() > 1 {
            state.used_approval_grants = staged_approval_grants;
            let results = self.execute_parallel_reads(prepared);
            if self.is_cancelled(input) {
                return ToolBatchControl::Cancelled;
            }
            return self.record_tool_results(input, state, results, false);
        }

        let Some(mut prepared) = prepared.pop() else {
            return ToolBatchControl::Continue;
        };
        if let Some(result) = prepared.rejection.take() {
            return self.record_tool_results(input, state, vec![(prepared, result)], false);
        }
        let decision = prepared
            .decision
            .clone()
            .expect("admitted tool call has a policy decision");
        if let ToolBrokerDecision::Ask {
            approval_request_id,
            reason,
        } = &decision
        {
            state.approval_requests.push(approval_request(
                input,
                approval_request_id,
                &prepared.call,
                reason,
                &self.policy.profile,
            ));
            state
                .pending_tool_calls
                .push(PendingToolCall::new_with_profile(
                    input,
                    &prepared.call,
                    &self.policy.profile,
                ));
            let pending = state
                .pending_tool_calls
                .last()
                .expect("pending tool call was just inserted")
                .clone();
            state
                .approval_checkpoints
                .push(state.checkpoint(input, &pending, next_model_turn));
            let result = self.execute_tool(&prepared.call, decision, state);
            state.tool_results.push(result);
            return ToolBatchControl::Blocked;
        }
        state.used_approval_grants = staged_approval_grants;
        let result = self.execute_tool(&prepared.call, decision, state);
        self.record_tool_results(input, state, vec![(prepared, result)], false)
    }

    /// 在策略评估前规范化一个模型调用，并校验其可执行输入。
    fn prepare_tool_call(
        &self,
        execution_call: &ModelToolCall,
        fingerprint: &str,
        invalid_was_observed: bool,
        state: &mut AgentLoopState,
    ) -> PreparedToolCall {
        if execution_call.parse_status != ModelToolParseStatus::Valid {
            if !invalid_was_observed {
                state.recovery_metrics.invalid_tool_call_count = state
                    .recovery_metrics
                    .invalid_tool_call_count
                    .saturating_add(1);
            }
            let validation_code = match execution_call.parse_status {
                ModelToolParseStatus::InvalidJson => "invalid_json_arguments",
                ModelToolParseStatus::SchemaMismatch => "tool_schema_mismatch",
                ModelToolParseStatus::UnknownTool => "tool_not_visible",
                ModelToolParseStatus::Valid => unreachable!("validated tool call"),
            };
            return PreparedToolCall {
                call: execution_call.clone(),
                fingerprint: fingerprint.to_string(),
                execution_mode: None,
                decision: None,
                rejection: Some(invalid_tool_arguments_result(
                    execution_call,
                    ToolInputValidationError::new(validation_code),
                    self.tool_broker.get(&execution_call.tool_name),
                )),
            };
        }
        let (execution_mode, execution_arguments) = match self
            .tool_broker
            .prepare_model_input(&execution_call.tool_name, &execution_call.arguments)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                if !invalid_was_observed {
                    state.recovery_metrics.invalid_tool_call_count = state
                        .recovery_metrics
                        .invalid_tool_call_count
                        .saturating_add(1);
                }
                return PreparedToolCall {
                    call: execution_call.clone(),
                    fingerprint: fingerprint.to_string(),
                    execution_mode: None,
                    decision: None,
                    rejection: Some(invalid_tool_arguments_result(
                        execution_call,
                        error,
                        self.tool_broker.get(&execution_call.tool_name),
                    )),
                };
            }
        };
        let mut bound_call = execution_call.clone();
        bound_call.raw_arguments = execution_arguments.to_string();
        bound_call.arguments = execution_arguments;
        let call = match bind_tool_call_to_profile(&bound_call, &self.policy.profile) {
            Ok(call) => call,
            Err(CommandBindingError::InvalidArguments(_)) => {
                if !invalid_was_observed {
                    state.recovery_metrics.invalid_tool_call_count = state
                        .recovery_metrics
                        .invalid_tool_call_count
                        .saturating_add(1);
                }
                return PreparedToolCall {
                    call: execution_call.clone(),
                    fingerprint: fingerprint.to_string(),
                    execution_mode: Some(execution_mode),
                    decision: None,
                    rejection: Some(invalid_tool_arguments_result(
                        execution_call,
                        ToolInputValidationError::new(command_argument_validation_code(
                            execution_call,
                        )),
                        self.tool_broker.get(&execution_call.tool_name),
                    )),
                };
            }
        };
        if let Err(error) = self
            .tool_broker
            .validate_execution_input(&call.tool_name, &call.arguments)
        {
            if !invalid_was_observed {
                state.recovery_metrics.invalid_tool_call_count = state
                    .recovery_metrics
                    .invalid_tool_call_count
                    .saturating_add(1);
            }
            return PreparedToolCall {
                call,
                fingerprint: fingerprint.to_string(),
                execution_mode: Some(execution_mode),
                decision: None,
                rejection: Some(invalid_tool_arguments_result(
                    execution_call,
                    error,
                    self.tool_broker.get(&execution_call.tool_name),
                )),
            };
        }
        if let Some(rejection) = self.workspace_preflight_rejection(&call) {
            return PreparedToolCall {
                call,
                fingerprint: fingerprint.to_string(),
                execution_mode: Some(execution_mode),
                decision: None,
                rejection: Some(rejection),
            };
        }
        PreparedToolCall {
            call,
            fingerprint: fingerprint.to_string(),
            execution_mode: Some(execution_mode),
            decision: None,
            rejection: None,
        }
    }

    fn workspace_preflight_rejection(&self, call: &ModelToolCall) -> Option<ToolResult> {
        if call.tool_name == UPDATE_PLAN_TOOL {
            return None;
        }
        let envelope = tool_call_request(call);
        let workspace_tools = self.workspace_tools.as_ref()?;
        workspace_tools
            .preflight(&call.tool_name, &call.arguments)
            .err()
            .map(|error| {
                let output = if call.tool_name == TOOL_COMMAND {
                    match command_tool_input(&call.arguments) {
                        Ok(input) => command_workspace_tool_failure(
                            &input,
                            error.into(),
                            &self.policy.profile,
                        ),
                        Err(_) => workspace_tool_failure(error.into()),
                    }
                } else {
                    workspace_tool_failure(error.into())
                };
                ToolResult::from_result(&envelope, &output)
            })
    }

    fn batch_rejection_result(&self, prepared: &PreparedToolCall) -> ToolResult {
        if let Some(result) = &prepared.rejection {
            return result.clone();
        }
        let envelope = tool_call_request(&prepared.call);
        let mut result = match prepared.decision.as_ref() {
            Some(decision @ ToolBrokerDecision::Ask { .. }) => {
                self.decision_result(&prepared.call, decision)
            }
            _ if prepared.execution_mode == Some(ToolExecutionMode::Exclusive) => {
                ToolResult::failed_with_kind(
                    &envelope,
                    ToolFailureKind::Capability,
                    "exclusive_tool_requires_single_call",
                    "state-changing and approval-sensitive tools must be submitted alone",
                )
            }
            _ => ToolResult::failed_with_kind(
                &envelope,
                ToolFailureKind::Capability,
                "tool_batch_rejected",
                "the tool batch was rejected before execution",
            ),
        };
        if prepared.call.tool_name == TOOL_COMMAND && result.audit_metadata().is_none() {
            let decision = prepared
                .decision
                .as_ref()
                .unwrap_or(&ToolBrokerDecision::Allow);
            result = result.with_audit(command_audit_metadata(
                None,
                &prepared.call,
                decision,
                self.policy.profile.approval_policy,
                &self.policy.profile,
            ));
        }
        result
    }

    fn decision_result(&self, call: &ModelToolCall, decision: &ToolBrokerDecision) -> ToolResult {
        let mut result =
            self.tool_broker
                .execute(&tool_call_request(call), decision.clone(), |_| {
                    unreachable!("deny and ask decisions never invoke the executor")
                });
        if call.tool_name == TOOL_COMMAND {
            result = result.with_audit(command_audit_metadata(
                None,
                call,
                decision,
                self.policy.profile.approval_policy,
                &self.policy.profile,
            ));
        }
        result
    }

    fn execute_parallel_reads(
        &self,
        prepared: Vec<PreparedToolCall>,
    ) -> Vec<(PreparedToolCall, ToolResult)> {
        let broker = &self.tool_broker;
        let workspace_tools = self.workspace_tools.as_ref();
        let cancellation = &self.cancellation;
        let results = parallel_map(prepared.clone(), |worker| {
            let decision = worker
                .decision
                .clone()
                .expect("admitted parallel read has a policy decision");
            let envelope = tool_call_request(&worker.call);
            broker.execute(&envelope, decision.clone(), |_| {
                execute_workspace_tool_call(
                    workspace_tools,
                    cancellation,
                    &worker.call,
                    &decision,
                    &PermissionProfile::workspace_write("."),
                )
            })
        });
        prepared
            .into_iter()
            .zip(results)
            .map(|(backup, result)| {
                let result = result.unwrap_or_else(|| {
                    ToolResult::failed_with_kind(
                        &tool_call_request(&backup.call),
                        ToolFailureKind::Infrastructure,
                        "parallel_read_worker_failed",
                        "parallel read worker failed",
                    )
                });
                (backup, result)
            })
            .collect()
    }

    /// 将 tool 结果送入完成和修复状态，同时保留失败类别。
    fn record_tool_results(
        &self,
        input: &AgentLoopInput,
        state: &mut AgentLoopState,
        results: Vec<(PreparedToolCall, ToolResult)>,
        approval_is_recoverable: bool,
    ) -> ToolBatchControl {
        let mut failure = None;
        let mut repairable_failure = None;
        for (prepared, result) in results {
            let result = if self.is_cancelled(input) && result.ok {
                cancelled_tool_result(&prepared.call)
            } else {
                result
            };
            let recoverable = is_repairable_tool_result(&result)
                || (approval_is_recoverable
                    && result.failure_kind == Some(ToolFailureKind::Approval));
            let non_repairable_error = (!result.ok && !recoverable).then(|| {
                result
                    .error_code
                    .clone()
                    .unwrap_or_else(|| "tool_execution_failed".to_string())
            });
            let recovery_feedback = state.observe_tool_result(&result, &prepared.fingerprint);
            if !result.ok && is_repairable_tool_result(&result) {
                repairable_failure = state.last_repair_failure.clone();
            }
            let provider_tool_name = (prepared.call.parse_status != ModelToolParseStatus::Valid)
                .then_some(PROVIDER_HISTORY_REJECTED_TOOL);
            state
                .messages
                .push(tool_result_message(&result, provider_tool_name));
            state.tool_results.push(result);
            if let Some(feedback) = recovery_feedback {
                state
                    .messages
                    .push(ModelMessage::text(ModelRole::Developer, feedback));
            }
            if failure.is_none() {
                failure = non_repairable_error;
            }
        }
        if let Some(repairable_failure) = repairable_failure {
            state.last_repair_failure = Some(repairable_failure);
        }
        if self.is_cancelled(input) {
            ToolBatchControl::Cancelled
        } else if let Some(error_code) = failure {
            ToolBatchControl::Failed(format!("tool execution failed: {error_code}"))
        } else {
            ToolBatchControl::Continue
        }
    }

    fn execute_tool(
        &self,
        call: &ModelToolCall,
        decision: ToolBrokerDecision,
        state: &mut AgentLoopState,
    ) -> ToolResult {
        let envelope = tool_call_request(call);
        let executor_decision = decision.clone();
        let mut result = self
            .tool_broker
            .execute(&envelope, decision.clone(), |_envelope| {
                if call.tool_name == UPDATE_PLAN_TOOL {
                    self.execute_plan_update(call, state)
                } else {
                    self.execute_workspace_tool(call, &executor_decision)
                }
            });
        if call.tool_name == TOOL_COMMAND {
            let existing_audit = result.audit_metadata().cloned();
            result = result.with_audit(command_audit_metadata(
                existing_audit.as_ref(),
                call,
                &decision,
                self.policy.profile.approval_policy,
                &self.policy.profile,
            ));
        }
        result
    }

    fn execute_plan_update(&self, call: &ModelToolCall, state: &mut AgentLoopState) -> ToolOutput {
        let plan = match update_plan_tool_input(&call.arguments) {
            Ok(plan) => plan,
            Err(error) => {
                return ToolOutput::failure(
                    "invalid_tool_arguments",
                    json!({"summary": error.to_string()}),
                );
            }
        };
        state.plan_update_count = state.plan_update_count.saturating_add(1);
        state.plan = Some(plan.clone());
        ToolOutput::success(json!({
            "plan": safe_plan_summary(&plan),
            "plan_update_count": state.plan_update_count,
        }))
    }

    fn execute_workspace_tool(
        &self,
        call: &ModelToolCall,
        decision: &ToolBrokerDecision,
    ) -> ToolOutput {
        execute_workspace_tool_call(
            self.workspace_tools.as_ref(),
            &self.cancellation,
            call,
            decision,
            &self.policy.profile,
        )
    }
}

fn execute_workspace_tool_call(
    workspace_tools: Option<&WorkspaceTools>,
    cancellation: &CancellationToken,
    call: &ModelToolCall,
    decision: &ToolBrokerDecision,
    profile: &PermissionProfile,
) -> ToolOutput {
    if cancellation.is_cancelled() {
        return ToolOutput::failure_with_kind(
            ToolFailureKind::Cancelled,
            "tool_cancelled",
            json!({"summary": "tool execution cancelled"}),
        );
    }
    let Some(workspace_tools) = workspace_tools else {
        return ToolOutput::failure_with_kind(
            ToolFailureKind::Backend,
            "backend_unavailable",
            json!({"summary": "workspace tool backend is unavailable"}),
        );
    };
    let result = match call.tool_name.as_str() {
        TOOL_READ => read_tool_input(&call.arguments).and_then(|input| {
            workspace_tools
                .read_cancellable(input, cancellation)
                .map_err(Into::into)
        }),
        TOOL_LIST => list_tool_input(&call.arguments).and_then(|input| {
            workspace_tools
                .list_cancellable(input, cancellation)
                .map_err(Into::into)
        }),
        TOOL_GREP => grep_tool_input(&call.arguments).and_then(|input| {
            workspace_tools
                .grep_cancellable(input, cancellation)
                .map_err(Into::into)
        }),
        TOOL_EDIT => edit_tool_input(&call.arguments)
            .and_then(|input| workspace_tools.edit(input, decision).map_err(Into::into)),
        TOOL_PATCH => patch_tool_input(&call.arguments)
            .and_then(|input| workspace_tools.patch(input, decision).map_err(Into::into)),
        TOOL_COMMAND => match command_tool_input(&call.arguments) {
            Ok(input) => {
                let (filesystem, network) = effective_command_policy(profile);
                Ok(workspace_tools
                    .command_cancellable_with_policy(
                        input.clone(),
                        filesystem,
                        network,
                        cancellation,
                    )
                    .map_err(Into::into)
                    .unwrap_or_else(|error| command_workspace_tool_failure(&input, error, profile)))
            }
            Err(error) => Err(error),
        },
        _ => Ok(ToolOutput::failure_with_kind(
            ToolFailureKind::Backend,
            "backend_unavailable",
            json!({"summary": "tool backend is unavailable"}),
        )),
    };
    result.unwrap_or_else(workspace_tool_failure)
}

fn parallel_map<T, R, F>(items: Vec<T>, worker: F) -> Vec<Option<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    std::thread::scope(|scope| {
        let worker = &worker;
        items
            .into_iter()
            .map(|item| scope.spawn(move || worker(item)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().ok())
            .collect()
    })
}

#[derive(Debug, Error)]
enum AgentLoopToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("{0}")]
    Workspace(#[from] WorkspaceToolError),
}

/// 为模型提供方请求选择公开上下文时使用的优先级。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AgentContextItemPriority {
    System,
    CurrentTurn,
    Evidence,
    History,
}

impl AgentContextItemPriority {
    fn rank(&self) -> u8 {
        match self {
            Self::CurrentTurn => 0,
            Self::System => 1,
            Self::Evidence => 2,
            Self::History => 3,
        }
    }
}

/// 在可见性允许时可以投影到模型历史中的上下文条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentContextItem {
    pub item_id: String,
    pub role: String,
    pub content: String,
    pub priority: AgentContextItemPriority,
    pub token_count: u32,
    pub public: bool,
    pub evaluator_only: bool,
}

impl AgentContextItem {
    /// 构造用户上下文项。
    pub fn user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            item_id: item_id.into(),
            role: USER_MESSAGE_ROLE.to_string(),
            token_count: approximate_token_count(&content),
            content,
            priority: AgentContextItemPriority::CurrentTurn,
            public: true,
            evaluator_only: false,
        }
    }

    /// 构造历史用户消息上下文项。
    pub fn history_user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::history(item_id, content, USER_MESSAGE_ROLE)
    }

    /// 构造历史 assistant 消息上下文项。
    pub fn history_assistant(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::history(item_id, content, ASSISTANT_MESSAGE_ROLE)
    }

    fn history(item_id: impl Into<String>, content: impl Into<String>, role: &'static str) -> Self {
        let item_id = item_id.into();
        let content = content.into();
        Self {
            item_id,
            role: role.to_string(),
            token_count: approximate_token_count(&content),
            content,
            priority: AgentContextItemPriority::History,
            public: true,
            evaluator_only: false,
        }
    }

    fn into_safe_history(self) -> Option<Self> {
        if self.priority != AgentContextItemPriority::History || !self.public || self.evaluator_only
        {
            return None;
        }
        match self.role.as_str() {
            USER_MESSAGE_ROLE => Some(Self::history_user(self.item_id, self.content)),
            ASSISTANT_MESSAGE_ROLE => Some(Self::history_assistant(self.item_id, self.content)),
            _ => None,
        }
    }
}

fn approximate_token_count(content: &str) -> u32 {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for character in content.chars() {
        if character.is_ascii() {
            ascii_chars = ascii_chars.saturating_add(1);
        } else {
            non_ascii_chars = non_ascii_chars.saturating_add(1);
        }
    }
    let ascii_tokens = ascii_chars.saturating_add(APPROXIMATE_ASCII_CHARS_PER_TOKEN - 1)
        / APPROXIMATE_ASCII_CHARS_PER_TOKEN;
    let estimated = ascii_tokens.saturating_add(non_ascii_chars);
    u32::try_from(estimated.max(1)).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone)]
struct ContextBudget {
    model_context_window: u32,
    reserved_output_tokens: u32,
    fixed_overhead_tokens: u32,
    developer_instruction_tokens: u32,
    tool_tokens: u32,
    message_framing_tokens: u32,
    input_token_budget: u32,
}

impl ContextBudget {
    fn reserved_request_tokens(&self) -> u32 {
        self.reserved_output_tokens
            .saturating_add(self.fixed_overhead_tokens)
            .saturating_add(self.developer_instruction_tokens)
            .saturating_add(self.tool_tokens)
            .saturating_add(self.message_framing_tokens)
    }

    fn metadata(&self, message_tokens: u32) -> Value {
        json!({
            "model_context_window": self.model_context_window,
            "input_token_budget": self.input_token_budget,
            "reserved_output_tokens": self.reserved_output_tokens,
            "fixed_overhead_tokens": self.fixed_overhead_tokens,
            "developer_instruction_tokens": self.developer_instruction_tokens,
            "tool_tokens": self.tool_tokens,
            "message_framing_tokens": self.message_framing_tokens,
            "reserved_request_tokens": self.reserved_request_tokens(),
            "message_tokens": message_tokens,
        })
    }

    fn for_public_assembly(max_tokens: u32) -> Self {
        Self {
            model_context_window: DEFAULT_MAX_CONTEXT_TOKENS,
            reserved_output_tokens: 0,
            fixed_overhead_tokens: 0,
            developer_instruction_tokens: 0,
            tool_tokens: 0,
            message_framing_tokens: 0,
            input_token_budget: max_tokens,
        }
    }
}

fn output_token_reservation(
    input: &AgentLoopInput,
    capabilities: &ProviderProtocolContract,
) -> Result<u32, String> {
    match input.model_preferences.max_output_tokens {
        Some(requested) if requested > capabilities.max_output_tokens => Err(format!(
            "requested output tokens ({requested}) exceed provider output limit ({})",
            capabilities.max_output_tokens
        )),
        Some(requested) => Ok(requested),
        None => Ok(DEFAULT_MAX_OUTPUT_TOKENS.min(capabilities.max_output_tokens)),
    }
}

fn effective_max_tool_calls(capabilities: &ProviderProtocolContract) -> u32 {
    if capabilities.supports_parallel_tool_calls {
        MAX_PARALLEL_READ_TOOL_CALLS
    } else {
        1
    }
}

/// 预留输出、指令、tool 和封装开销后计算请求预算。
fn context_budget(
    input: &AgentLoopInput,
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
    max_tool_calls: u32,
) -> Result<ContextBudget, String> {
    if capabilities.max_context_tokens == 0 || capabilities.max_output_tokens == 0 {
        return Err("provider token capabilities must be greater than zero".to_string());
    }
    let developer_instruction_tokens =
        approximate_token_count(&developer_instructions(input, max_tool_calls));
    let tool_tokens = reserved_model_tool_tokens(loop_tools, capabilities)?;
    let message_count = u32::try_from(input.input.len().saturating_add(1)).unwrap_or(u32::MAX);
    let message_framing_tokens = message_count.saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    let reserved_output_tokens = output_token_reservation(input, capabilities)?;
    let fixed_overhead_tokens = MODEL_REQUEST_FIXED_OVERHEAD_TOKENS;
    let reserved_request_tokens = reserved_output_tokens
        .saturating_add(fixed_overhead_tokens)
        .saturating_add(developer_instruction_tokens)
        .saturating_add(tool_tokens)
        .saturating_add(message_framing_tokens);
    if reserved_request_tokens >= capabilities.max_context_tokens {
        return Err(
            "provider context window cannot fit the reserved output and request overhead"
                .to_string(),
        );
    }

    Ok(ContextBudget {
        model_context_window: capabilities.max_context_tokens,
        reserved_output_tokens,
        fixed_overhead_tokens,
        developer_instruction_tokens,
        tool_tokens,
        message_framing_tokens,
        input_token_budget: capabilities
            .max_context_tokens
            .saturating_sub(reserved_request_tokens),
    })
}

fn model_request_fits_context(
    tools: &[ModelToolSchema],
    messages: &[ModelMessage],
    budget: &ContextBudget,
) -> bool {
    model_request_token_count(tools, messages, budget) <= budget.model_context_window
}

fn model_request_token_count(
    tools: &[ModelToolSchema],
    messages: &[ModelMessage],
    budget: &ContextBudget,
) -> u32 {
    let projected_messages = messages
        .iter()
        .map(|message| {
            let content = &message.content;
            let tool_calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.tool_call_id,
                        "name": call.tool_name,
                        "arguments": call.raw_arguments,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "role": message.role,
                "content": content,
                "tool_call_id": message.tool_call_id,
                "tool_calls": tool_calls,
            })
        })
        .collect::<Vec<_>>();
    let projected_tools = tools
        .iter()
        .map(|tool| {
            json!({
                "name": &tool.name,
                "description": &tool.description,
                "parameters": &tool.parameters_schema,
            })
        })
        .collect::<Vec<_>>();
    let payload_tokens = serde_json::to_string(&(projected_messages, projected_tools))
        .map_or(u32::MAX, |payload| approximate_token_count(&payload));
    let message_framing = u32::try_from(messages.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    payload_tokens
        .saturating_add(budget.reserved_output_tokens)
        .saturating_add(message_framing)
        .saturating_add(budget.fixed_overhead_tokens)
}

#[derive(Debug)]
struct ContextCompactionOutcome {
    messages: Vec<ModelMessage>,
    compacted_message_count: u32,
    before_tokens: u32,
    after_tokens: u32,
}

/// 压缩此前的模型消息，同时保留当前控制信息和校验证据。
fn compact_model_messages(
    tools: &[ModelToolSchema],
    state: &AgentLoopState,
    budget: &ContextBudget,
) -> Option<ContextCompactionOutcome> {
    let before_tokens = model_request_token_count(tools, &state.messages, budget);
    if before_tokens <= budget.model_context_window {
        return None;
    }

    let authority_indices = state
        .messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| matches!(message.role, ModelRole::System).then_some(index))
        .chain(
            state
                .messages
                .iter()
                .position(|message| message.role == ModelRole::Developer),
        )
        .collect::<BTreeSet<_>>();
    let current_user_index = state
        .messages
        .iter()
        .rposition(|message| message.role == ModelRole::User)?;
    let latest_tool_pair = latest_complete_tool_pair(&state.messages);

    let mut preserved_indices = authority_indices.clone();
    preserved_indices.insert(current_user_index);
    if let Some((assistant_index, _tool_index)) = latest_tool_pair {
        preserved_indices.insert(assistant_index);
    }
    let compacted_message_count =
        u32::try_from(state.messages.len().saturating_sub(preserved_indices.len()))
            .unwrap_or(u32::MAX);
    if compacted_message_count == 0 {
        return None;
    }

    let mut messages = authority_indices
        .iter()
        .map(|index| state.messages[*index].clone())
        .collect::<Vec<_>>();
    messages.push(ModelMessage::text(
        ModelRole::Developer,
        compaction_summary(state, compacted_message_count),
    ));
    messages.push(state.messages[current_user_index].clone());
    if let Some((assistant_index, tool_index)) = latest_tool_pair
        && assistant_index > current_user_index
    {
        messages.push(state.messages[assistant_index].clone());
        messages.push(compacted_tool_result_message(
            &state.messages[tool_index],
            &state.tool_results,
        ));
    }

    let after_tokens = model_request_token_count(tools, &messages, budget);
    if after_tokens >= before_tokens || after_tokens > budget.model_context_window {
        return None;
    }
    Some(ContextCompactionOutcome {
        messages,
        compacted_message_count,
        before_tokens,
        after_tokens,
    })
}

fn latest_complete_tool_pair(messages: &[ModelMessage]) -> Option<(usize, usize)> {
    for tool_index in (0..messages.len()).rev() {
        let tool_message = &messages[tool_index];
        if tool_message.role != ModelRole::Tool {
            continue;
        }
        let Some(tool_call_id) = tool_message.tool_call_id.as_deref() else {
            continue;
        };
        if let Some(assistant_index) = (0..tool_index).rev().find(|index| {
            let message = &messages[*index];
            message.role == ModelRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_call_id == tool_call_id)
        }) {
            return Some((assistant_index, tool_index));
        }
    }
    None
}

fn compacted_tool_result_message(
    original: &ModelMessage,
    tool_results: &[ToolResult],
) -> ModelMessage {
    let tool_result = original.tool_call_id.as_deref().and_then(|tool_call_id| {
        tool_results
            .iter()
            .rev()
            .find(|result| result.tool_call_id == tool_call_id)
    });
    let content = json!({
        "compacted": true,
        "ok": tool_result.is_some_and(|result| result.ok),
        "error_code": tool_result.and_then(|result| result.error_code.as_deref()),
        "truncated": tool_result.is_some_and(|result| result.truncated),
        "instruction": "The prior tool output was omitted to fit the context window. Re-read the relevant file or rerun a safe command if exact output is needed."
    });
    let mut message = ModelMessage::text(ModelRole::Tool, content.to_string());
    message.tool_call_id = original.tool_call_id.clone();
    message
}

fn compaction_summary(state: &AgentLoopState, compacted_message_count: u32) -> String {
    let verification = state.completion.summary();
    let active_control_instructions = compaction_control_instructions(state);
    let plan = state.plan.as_ref().map(|plan| {
        let completed = plan
            .steps
            .iter()
            .filter(|step| step.status == AgentPlanStepStatus::Completed)
            .count();
        let in_progress = plan
            .steps
            .iter()
            .find(|step| step.status == AgentPlanStepStatus::InProgress);
        let next_pending = plan
            .steps
            .iter()
            .find(|step| step.status == AgentPlanStepStatus::Pending);
        let current_step = in_progress.or(next_pending).map(|step| {
            safe_plan_step_text(&step.step)
                .chars()
                .take(MAX_COMPACTION_PLAN_STEP_CHARS)
                .collect::<String>()
        });
        json!({
            "step_count": plan.steps.len(),
            "completed_step_count": completed,
            "current_step": current_step,
        })
    });
    let failed_tool_result_count = state
        .tool_results
        .iter()
        .filter(|result| !result.ok)
        .count();
    json!({
        "type": "agent_context_compaction",
        "notice": "Older messages and raw tool output were omitted. Do not assume omitted evidence; inspect the workspace again when needed.",
        "compacted_message_count": compacted_message_count,
        "tool_result_count": state.tool_results.len(),
        "failed_tool_result_count": failed_tool_result_count,
        "plan": plan,
        "verification": {
            "required": verification.required,
            "passed": verification.passed,
            "required_command_count": verification.required_command_count,
            "satisfied_command_count": verification.satisfied_command_count,
            "unresolved_failures": verification.unresolved_failures.into_iter().take(8).collect::<Vec<_>>(),
        },
        "recovery": &state.recovery_metrics,
        "active_control_instructions": active_control_instructions,
    })
    .to_string()
}

fn compaction_control_instructions(state: &AgentLoopState) -> Vec<String> {
    let mut instructions = Vec::new();
    if !state.completion.allows_final() {
        instructions.push(state.completion.feedback());
    }
    if state.plan.as_ref().is_some_and(|plan| !plan.is_completed()) {
        instructions.push(PLAN_COMPLETION_REQUIRED.to_string());
    }
    if state
        .last_repair_failure
        .as_ref()
        .is_some_and(|failure| failure.consecutive_count >= 2)
    {
        instructions.push(REPEATED_FAILURE_RECOVERY_INSTRUCTIONS.to_string());
    }
    instructions
}

/// 选择符合请求令牌预算的公开当前 turn 条目和最新历史。
pub fn assemble_context_items(items: &[AgentContextItem], max_tokens: u32) -> ContextBundle {
    let budget = ContextBudget::for_public_assembly(max_tokens);
    assemble_context_items_with_budget(items, &budget)
}

fn assemble_context_items_with_budget(
    items: &[AgentContextItem],
    budget: &ContextBudget,
) -> ContextBundle {
    let max_tokens = budget.input_token_budget;
    let mut candidates: Vec<(usize, &AgentContextItem)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.public
                && !item.evaluator_only
                && item.priority != AgentContextItemPriority::History
        })
        .collect();
    candidates.sort_by_key(|(index, item)| (item.priority.rank(), *index));

    let mut used_tokens = 0;
    let mut included_indices = HashSet::new();
    for (index, item) in candidates {
        if item.token_count > max_tokens.saturating_sub(used_tokens) {
            continue;
        }
        used_tokens = used_tokens.saturating_add(item.token_count);
        included_indices.insert(index);
    }

    let history_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.public
                && !item.evaluator_only
                && item.priority == AgentContextItemPriority::History
        })
        .map(|(index, _)| index)
        .collect();
    let mut history_end = history_indices.len();
    while history_end > 0 {
        let mut history_start = history_end - 1;
        let newest = &items[history_indices[history_start]];
        if newest.role == ASSISTANT_MESSAGE_ROLE
            && history_start > 0
            && items[history_indices[history_start - 1]].role == USER_MESSAGE_ROLE
        {
            history_start -= 1;
        }
        let group_tokens = history_indices[history_start..history_end]
            .iter()
            .fold(0u32, |total, index| {
                total.saturating_add(items[*index].token_count)
            });
        if group_tokens > max_tokens.saturating_sub(used_tokens) {
            break;
        }
        used_tokens = used_tokens.saturating_add(group_tokens);
        included_indices.extend(history_indices[history_start..history_end].iter().copied());
        history_end = history_start;
    }

    let mut included_item_ids = Vec::new();
    let mut excluded_item_ids = Vec::new();
    let mut messages = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !included_indices.contains(&index) {
            excluded_item_ids.push(item.item_id.clone());
            continue;
        }
        included_item_ids.push(item.item_id.clone());
        messages.push(json!({
            "role": item.role,
            "content": item.content,
        }));
    }

    ContextBundle {
        messages,
        included_item_ids,
        excluded_item_ids,
        budget: budget.metadata(used_tokens),
    }
}

fn current_turn_excluded(input: &AgentLoopInput, context: &ContextBundle) -> bool {
    input.input.iter().any(|item| {
        item.priority == AgentContextItemPriority::CurrentTurn
            && item.public
            && !item.evaluator_only
            && !context.included_item_ids.contains(&item.item_id)
    })
}

/// 在已批准调用执行前恢复并重新规范化检查点。
fn restore_checkpoint(
    input: &AgentLoopInput,
    pending: &PendingToolCall,
    payload: &Value,
    tool_broker: &ToolBroker,
    permission_profile: &PermissionProfile,
) -> Result<(AgentLoopState, u32), String> {
    let checkpoint: AgentLoopCheckpoint = serde_json::from_value(payload.clone())
        .map_err(|error| format!("invalid approval checkpoint: {error}"))?;
    if checkpoint.checkpoint_version != APPROVAL_CHECKPOINT_VERSION {
        return Err("unsupported approval checkpoint version".to_string());
    }
    if checkpoint.thread_id != input.thread_id {
        return Err("approval checkpoint thread mismatch".to_string());
    }
    if checkpoint.turn_id != input.turn_id {
        return Err("approval checkpoint turn mismatch".to_string());
    }
    if checkpoint.pending_tool_call != *pending {
        return Err("approval checkpoint tool call mismatch".to_string());
    }
    let expected_request_id =
        approval_request_id_from_tool_call_id(&input.turn_id, &pending.tool_call_id);
    if pending.request_id != expected_request_id
        || checkpoint.pending_tool_call.request_id != expected_request_id
    {
        return Err("approval checkpoint request mismatch".to_string());
    }
    if checkpoint.model_turns == 0 {
        return Err("approval checkpoint model-turn offset is invalid".to_string());
    }
    if checkpoint.approval_count == 0 {
        return Err("approval checkpoint approval count is invalid".to_string());
    }
    if checkpoint.messages.is_empty() {
        return Err("approval checkpoint messages are missing".to_string());
    }
    let last_message = checkpoint
        .messages
        .last()
        .ok_or_else(|| "approval checkpoint messages are missing".to_string())?;
    if last_message.role != ModelRole::Assistant
        || last_message.tool_calls.len() != 1
        || last_message.tool_calls[0].tool_call_id != pending.tool_call_id
    {
        return Err("approval checkpoint assistant tool-call ordering is invalid".to_string());
    }
    let model_visible_call = &last_message.tool_calls[0];
    if model_visible_call.parse_status != ModelToolParseStatus::Valid {
        return Err("approval checkpoint assistant tool-call name is invalid".to_string());
    }
    let pending_call = pending
        .to_model_tool_call()
        .map_err(|error| format!("invalid pending checkpoint tool call arguments: {error}"))?;
    if model_visible_call.tool_name != pending.tool_name {
        return Err("approval checkpoint assistant tool-call name is invalid".to_string());
    }
    let canonical_model_call = model_visible_call.clone();
    let canonical_model_call =
        canonicalize_model_tool_call(tool_broker, &canonical_model_call, permission_profile)
            .map_err(|error| format!("approval checkpoint tool call is invalid: {error}"))?;
    if canonical_model_call.tool_call_id != pending_call.tool_call_id
        || canonical_model_call.tool_name != pending_call.tool_name
        || canonical_model_call.arguments != pending_call.arguments
    {
        return Err(
            "approval checkpoint assistant tool-call arguments do not match pending call"
                .to_string(),
        );
    }
    let used_approval_grants = checkpoint
        .used_approval_grants
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if used_approval_grants.len() != checkpoint.used_approval_grants.len() {
        return Err("approval checkpoint contains duplicate grants".to_string());
    }
    if used_approval_grants.contains(&pending.request_id) {
        return Err("approval checkpoint consumed the pending grant".to_string());
    }
    if let Some(plan) = &checkpoint.plan {
        plan.validate()
            .map_err(|error| format!("approval checkpoint plan is invalid: {error}"))?;
        if checkpoint.plan_update_count == 0 {
            return Err("approval checkpoint plan update count is invalid".to_string());
        }
    } else if checkpoint.plan_update_count != 0 {
        return Err("approval checkpoint plan update count is invalid".to_string());
    }
    let seen_tool_call_fingerprints = checkpoint
        .seen_tool_call_fingerprints
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if seen_tool_call_fingerprints.len() != checkpoint.seen_tool_call_fingerprints.len()
        || seen_tool_call_fingerprints
            .iter()
            .any(|fingerprint| !is_sha256_fingerprint(fingerprint))
    {
        return Err("approval checkpoint tool-call fingerprint state is invalid".to_string());
    }
    if checkpoint
        .last_repair_failure
        .as_ref()
        .is_some_and(|failure| {
            failure.consecutive_count == 0 || !is_sha256_fingerprint(&failure.signature)
        })
    {
        return Err("approval checkpoint repair state is invalid".to_string());
    }
    if checkpoint.provider_attempts.retry_count > checkpoint.provider_attempts.attempt_count {
        return Err("approval checkpoint provider attempt state is invalid".to_string());
    }
    if checkpoint.context_trace.as_ref().is_some_and(|trace| {
        if trace.compaction_count == 0 {
            return trace.compacted_message_count != 0
                || trace.last_compaction_before_tokens.is_some()
                || trace.last_compaction_after_tokens.is_some();
        }
        trace.compacted_message_count < trace.compaction_count
            || trace.last_compaction_before_tokens.is_none()
            || trace.last_compaction_after_tokens.is_none()
            || trace.last_compaction_after_tokens >= trace.last_compaction_before_tokens
    }) {
        return Err("approval checkpoint context compaction state is invalid".to_string());
    }
    let tool_results = checkpoint
        .tool_results
        .into_iter()
        .map(CheckpointToolResult::into_tool_result)
        .collect::<Vec<_>>();
    let mut derived_completion =
        CompletionTracker::from_requirements(&input.verification_requirements)?;
    for tool_result in &tool_results {
        derived_completion.observe(tool_result);
    }
    if derived_completion != checkpoint.completion {
        return Err("approval checkpoint completion state mismatch".to_string());
    }
    let derived_plan_update_count = tool_results
        .iter()
        .filter(|tool_result| tool_result.tool_name == UPDATE_PLAN_TOOL && tool_result.ok)
        .count() as u32;
    if derived_plan_update_count != checkpoint.plan_update_count {
        return Err("approval checkpoint plan update count mismatch".to_string());
    }
    let mut state = AgentLoopState::new(checkpoint.messages, input.max_turns.max(1), None);
    state.tool_results = tool_results;
    state.used_approval_grants = used_approval_grants;
    state.prior_approval_count = checkpoint.approval_count;
    state.completion = derived_completion;
    state.last_completion_error = checkpoint.last_completion_error;
    state.plan = checkpoint.plan;
    state.plan_update_count = checkpoint.plan_update_count;
    state.recovery_metrics = checkpoint.recovery_metrics;
    state.model_usage = checkpoint.model_usage;
    state.provider_attempts = checkpoint.provider_attempts;
    state.context_trace = checkpoint.context_trace;
    state.seen_tool_call_fingerprints = seen_tool_call_fingerprints;
    state.last_repair_failure = checkpoint.last_repair_failure;
    Ok((state, checkpoint.model_turns))
}

fn model_response_validation_error(validation_errors: Vec<String>) -> ModelError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        format!(
            "{MODEL_RESPONSE_VALIDATION_ERROR}: {}",
            validation_errors.join(", ")
        ),
    )
    .with_provider_diagnostic(
        "provider_response_invalid",
        ProviderErrorStage::ResponseValidation,
    );
    error.validation_errors = validation_errors;
    error
}

fn failed_result(error: impl Into<String>) -> AgentLoopResult {
    AgentLoopResult {
        status: AgentStatus::Failed,
        completed: false,
        final_answer: None,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        approval_requests: Vec::new(),
        pending_tool_calls: Vec::new(),
        approval_checkpoints: Vec::new(),
        tool_results: Vec::new(),
        verification: AgentVerification::default(),
        plan: None,
        plan_update_count: 0,
        recovery_metrics: AgentRecoveryMetrics::default(),
        model_usage: ModelUsage::default(),
        provider_attempts: ProviderAttemptMetadata::default(),
        error: Some(error.into()),
        model_turn_limit: 0,
        context_trace: None,
        error_category: None,
        provider_diagnostic: None,
        provider_protocol_contract: None,
        provider_capability_metadata: None,
    }
}

/// 可直接交给模型提供方的上下文消息，以及用于追踪和诊断的纳入元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundle {
    pub messages: Vec<Value>,
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
}

/// 随运行持久化的追踪安全上下文选择和压缩计数器。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentContextTrace {
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
    #[serde(default)]
    pub compaction_count: u32,
    #[serde(default)]
    pub compacted_message_count: u32,
    #[serde(default)]
    pub last_compaction_before_tokens: Option<u32>,
    #[serde(default)]
    pub last_compaction_after_tokens: Option<u32>,
}

impl From<&ContextBundle> for AgentContextTrace {
    fn from(context: &ContextBundle) -> Self {
        Self {
            included_item_ids: context.included_item_ids.clone(),
            excluded_item_ids: context.excluded_item_ids.clone(),
            budget: context.budget.clone(),
            compaction_count: 0,
            compacted_message_count: 0,
            last_compaction_before_tokens: None,
            last_compaction_after_tokens: None,
        }
    }
}

impl AgentContextTrace {
    fn refresh_context(&mut self, context: &ContextBundle) {
        self.included_item_ids = context.included_item_ids.clone();
        self.excluded_item_ids = context.excluded_item_ids.clone();
        self.budget = context.budget.clone();
    }

    fn record_compaction(&mut self, outcome: &ContextCompactionOutcome) {
        self.compaction_count = self.compaction_count.saturating_add(1);
        self.compacted_message_count = self
            .compacted_message_count
            .saturating_add(outcome.compacted_message_count);
        self.last_compaction_before_tokens = Some(outcome.before_tokens);
        self.last_compaction_after_tokens = Some(outcome.after_tokens);
    }
}

struct ModelToolView {
    tools: Vec<ModelToolSchema>,
    visible_tool_names: Vec<String>,
    max_tool_calls: u32,
}

impl ModelToolView {
    fn finalization() -> Self {
        Self {
            tools: Vec::new(),
            visible_tool_names: Vec::new(),
            max_tool_calls: 0,
        }
    }
}

fn model_turn_request(
    input: &AgentLoopInput,
    budget: &ContextBudget,
    turn_index: u32,
    state: &AgentLoopState,
    tool_view: ModelToolView,
    capabilities: &ProviderProtocolContract,
    finalization_only: bool,
) -> ModelTurnRequest {
    let tools = tool_view.tools;
    let strict_tool_schema = !tools.is_empty()
        && capabilities.supports_strict_tool_schema
        && tools
            .iter()
            .all(|tool| is_strict_tool_schema_compatible(&tool.parameters_schema));
    let mut request = ModelTurnRequest {
        request_id: format!("model_request_{}_{}", input.turn_id, turn_index),
        messages: state.messages.clone(),
        tools,
        tool_choice: Default::default(),
        model_preferences: ModelPreferences {
            max_output_tokens: Some(budget.reserved_output_tokens),
            ..input.model_preferences.clone()
        },
    };
    if finalization_only {
        request.tool_choice.mode = ToolChoiceMode::None;
    }
    request.tool_choice.max_tool_calls = tool_view.max_tool_calls;
    request.tool_choice.strict_tool_schema = strict_tool_schema;
    request
}

fn model_tool_schemas(loop_tools: &ToolBroker) -> Vec<ModelToolSchema> {
    loop_tools
        .tool_schema_payloads()
        .into_iter()
        .filter_map(|tool| {
            Some(ModelToolSchema {
                name: tool.get("name")?.as_str()?.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters_schema: tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

fn visible_model_tool_schemas(loop_tools: &ToolBroker) -> Vec<ModelToolSchema> {
    model_tool_schemas(loop_tools)
}

fn model_tool_view(
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
    max_tool_calls: u32,
) -> Result<ModelToolView, String> {
    if capabilities.max_tools_per_request == 0 {
        return Err("provider tool-definition limit must be greater than zero".to_string());
    }
    let visible_tools = visible_model_tool_schemas(loop_tools);
    let visible_tool_names = visible_tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    if visible_tools.len() > capabilities.max_tools_per_request as usize {
        return Err(format!(
            "provider direct tool-definition limit ({}) is below the required tool count ({})",
            capabilities.max_tools_per_request,
            visible_tools.len()
        ));
    }
    Ok(ModelToolView {
        tools: visible_tools,
        visible_tool_names,
        max_tool_calls,
    })
}

fn resolve_model_tool_calls(
    provider_calls: &[ModelToolCall],
    visible_tool_names: &[String],
) -> Vec<ModelToolCall> {
    provider_calls
        .iter()
        .map(|call| {
            if call.parse_status != ModelToolParseStatus::Valid {
                return call.clone();
            }
            let mut resolved = call.clone();
            if resolved.parse_status == ModelToolParseStatus::Valid
                && !visible_tool_names
                    .iter()
                    .any(|tool_name| tool_name == &resolved.tool_name)
            {
                resolved.parse_status = ModelToolParseStatus::UnknownTool;
            }
            resolved
        })
        .collect()
}

fn model_tool_payload_tokens(tools: &[ModelToolSchema]) -> u32 {
    serde_json::to_string(tools).map_or(u32::MAX, |payload| approximate_token_count(&payload))
}

fn reserved_model_tool_tokens(
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
) -> Result<u32, String> {
    let visible_tools = visible_model_tool_schemas(loop_tools);
    if visible_tools.len() > capabilities.max_tools_per_request as usize {
        return Err(format!(
            "provider direct tool-definition limit ({}) is below the required tool count ({})",
            capabilities.max_tools_per_request,
            visible_tools.len()
        ));
    }
    Ok(model_tool_payload_tokens(&visible_tools))
}

fn model_messages_from_input(
    input: &AgentLoopInput,
    context: &ContextBundle,
    max_tool_calls: u32,
) -> Vec<ModelMessage> {
    let mut messages = vec![ModelMessage::text(
        ModelRole::Developer,
        developer_instructions(input, max_tool_calls),
    )];
    messages.extend(model_messages_from_context(context));
    messages
}

fn developer_instructions(input: &AgentLoopInput, max_tool_calls: u32) -> String {
    let tool_call_instruction = if max_tool_calls == 1 {
        "Issue at most one tool call per assistant response and wait for its result.".to_string()
    } else {
        format!(
            "Issue up to {max_tool_calls} tool calls in one response only when every call is an independent read-only operation. Issue mutations, commands, plan updates, approval-sensitive calls, and calls that depend on earlier results one at a time and wait for each result."
        )
    };
    let instructions = format!("{AGENT_DEVELOPER_INSTRUCTIONS} {tool_call_instruction}");
    match input.project_instructions.as_deref() {
        Some(project) => {
            format!("{instructions}\n\nProject instructions:\n{project}")
        }
        None => instructions,
    }
}

fn refresh_developer_instructions(
    messages: &mut [ModelMessage],
    input: &AgentLoopInput,
    max_tool_calls: u32,
) {
    if let Some(message) = messages
        .iter_mut()
        .find(|message| message.role == ModelRole::Developer)
    {
        message.content = developer_instructions(input, max_tool_calls);
    }
}

fn model_messages_from_context(context: &ContextBundle) -> Vec<ModelMessage> {
    context
        .messages
        .iter()
        .filter_map(|message| {
            let role = match message.get("role").and_then(Value::as_str) {
                Some("system") => ModelRole::System,
                Some("developer") => ModelRole::Developer,
                Some("assistant") => ModelRole::Assistant,
                Some("tool") => ModelRole::Tool,
                _ => ModelRole::User,
            };
            message
                .get("content")
                .and_then(Value::as_str)
                .map(|content| ModelMessage::text(role, content))
        })
        .collect()
}

fn assistant_message_text(message: Option<&ModelMessage>) -> String {
    message
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

fn provider_history_assistant_message(
    original: Option<&ModelMessage>,
    model_visible_calls: &[ModelToolCall],
    execution_calls: &[ModelToolCall],
) -> ModelMessage {
    // 在 state/tool_results 中保留拒绝调用的诊断信息，但不把 provider 原始名称或参数重放到下一次请求。
    debug_assert_eq!(model_visible_calls.len(), execution_calls.len());
    let mut message = original
        .cloned()
        .unwrap_or_else(|| ModelMessage::assistant_tool_calls(Vec::new()));
    message.tool_calls = model_visible_calls
        .iter()
        .zip(execution_calls)
        .map(|(model_visible_call, execution_call)| {
            if execution_call.parse_status == ModelToolParseStatus::Valid {
                model_visible_call.clone()
            } else {
                ModelToolCall {
                    tool_call_id: model_visible_call.tool_call_id.clone(),
                    tool_name: PROVIDER_HISTORY_REJECTED_TOOL.to_string(),
                    arguments: json!({}),
                    raw_arguments: "{}".to_string(),
                    parse_status: ModelToolParseStatus::Valid,
                    validation_errors: Vec::new(),
                }
            }
        })
        .collect();
    message
}

fn tool_result_message(tool_result: &ToolResult, provider_tool_name: Option<&str>) -> ModelMessage {
    let mut payload = tool_result.to_message_payload();
    if let Some(provider_tool_name) = provider_tool_name {
        payload["tool_name"] = json!(provider_tool_name);
    }
    let mut message = ModelMessage::text(ModelRole::Tool, payload.to_string());
    message.tool_call_id = Some(tool_result.tool_call_id.clone());
    message
}

fn tool_call_request(call: &ModelToolCall) -> ToolCallRequest {
    // 执行 broker 校验解析后的可执行输入；provider 原始文本保留在模型消息和 approval checkpoint 中，不能定义执行器 payload。
    ToolCallRequest::new(
        call.tool_call_id.clone(),
        call.tool_name.clone(),
        serde_json::to_string(&call.arguments).expect("model tool arguments serialize"),
    )
}

fn audit_events_from_tool_results(tool_results: &[ToolResult]) -> Vec<Value> {
    tool_results
        .iter()
        .filter_map(|result| {
            let audit = project_audit_event(result.audit_metadata()?);
            (!audit.as_object().is_none_or(serde_json::Map::is_empty)).then_some(audit)
        })
        .collect()
}

/// 将内部 command 结果归约为可用于 verification/evaluation 的精确成功观察。
pub fn successful_command_scope_digest(tool_result: &ToolResult) -> Option<&str> {
    (tool_result.tool_name == TOOL_COMMAND && tool_result.ok)
        .then_some(tool_result.result_id.as_deref())
        .flatten()
        .filter(|digest| is_sha256_fingerprint(digest))
}

/// 返回最后一次 workspace mutation 之后、按原始结果顺序保留重复项的 command 观察。
pub fn eligible_command_scope_digests(tool_results: &[ToolResult]) -> Vec<String> {
    let first_eligible_result = tool_results
        .iter()
        .rposition(|tool_result| matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH))
        .map_or(0, |index| index + 1);
    tool_results[first_eligible_result..]
        .iter()
        .filter_map(successful_command_scope_digest)
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, Serialize)]
struct SafeAuditEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_validation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    argument_validation_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_provenance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_scope_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executor_started: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_process_fallback: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    network_access: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_evaluated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_scope_binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_enforcement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<SafeAuditTimeout>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum SafeAuditTimeout {
    Seconds(u64),
    Unknown(&'static str),
}

/// 将内部 WorkspaceTool/approval metadata 投影为普通 trace 可安全持久化的闭集字段。
///
/// 该函数构造全新的 typed projection；未知字段、绝对 cwd、raw arguments、reason、ID 和
/// 其他嵌套 JSON 均不会被复制到普通 trace 或 Evaluation agent-trace。
pub fn project_audit_event(metadata: &Value) -> Value {
    let object = metadata.as_object();
    let projection = SafeAuditEvent {
        argument_validation: allowed_label(object, "argument_validation", &["failed"]),
        argument_validation_code: bounded_audit_label(object, "argument_validation_code"),
        approval_decision: allowed_label(
            object,
            "approval_decision",
            &[
                "allowed_by_policy",
                "approved",
                "approval_required",
                "denied",
                "unavailable",
            ],
        ),
        approval_policy: allowed_label(object, "approval_policy", &["never", "on-request"]),
        command_provenance: allowed_label(object, "command_provenance", &["agent_requested"]),
        command_scope_digest: object
            .and_then(|fields| fields.get("command_scope_digest"))
            .and_then(Value::as_str)
            .filter(|digest| *digest == "unavailable" || is_sha256_fingerprint(digest))
            .map(str::to_string),
        executor_started: object_bool(object, "executor_started"),
        local_process_fallback: object_bool(object, "local_process_fallback"),
        network_access: allowed_label(object, "network_access", &["allowed", "denied", "unknown"]),
        policy_evaluated: object_bool(object, "policy_evaluated"),
        policy_scope_binding: allowed_label(
            object,
            "policy_scope_binding",
            &["bound", "unavailable"],
        ),
        sandbox_backend: bounded_audit_label(object, "sandbox_backend"),
        sandbox_enforcement: allowed_label(
            object,
            "sandbox_enforcement",
            &[
                "not_executed",
                "restricted_token",
                "strict",
                "unavailable",
                "unknown",
            ],
        ),
        sandbox_mode: allowed_label(
            object,
            "sandbox_mode",
            &["read_only", "unknown", "workspace_write"],
        ),
        timeout_seconds: object
            .and_then(|fields| fields.get("timeout_seconds"))
            .and_then(|value| {
                value
                    .as_u64()
                    .filter(|seconds| *seconds <= 3_600)
                    .map(SafeAuditTimeout::Seconds)
                    .or_else(|| {
                        (value.as_str() == Some("unknown"))
                            .then_some(SafeAuditTimeout::Unknown("unknown"))
                    })
            }),
    };
    serde_json::to_value(projection).expect("safe audit projection serializes")
}

fn object_bool(object: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<bool> {
    object
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_bool)
}

fn allowed_label(
    object: Option<&serde_json::Map<String, Value>>,
    key: &str,
    allowed: &[&str],
) -> Option<String> {
    let value = object
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_str)?;
    allowed.contains(&value).then(|| value.to_string())
}

fn bounded_audit_label(
    object: Option<&serde_json::Map<String, Value>>,
    key: &str,
) -> Option<String> {
    let value = object
        .and_then(|fields| fields.get(key))
        .and_then(Value::as_str)?;
    if value.is_empty()
        || value.len() > 64
        || contains_sensitive_text(value)
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        return None;
    }
    Some(value.to_string())
}

fn command_audit_metadata(
    existing: Option<&Value>,
    call: &ModelToolCall,
    decision: &ToolBrokerDecision,
    approval_policy: ApprovalPolicy,
    profile: &PermissionProfile,
) -> Value {
    let mut audit = existing.cloned().unwrap_or_else(|| json!({}));
    if let Ok(input) = command_tool_input(&call.arguments) {
        if audit.get("cwd").is_none() {
            audit["cwd"] = json!(input.effective_cwd());
        }
        if audit.get("timeout_seconds").is_none() {
            audit["timeout_seconds"] = json!(input.effective_timeout_seconds());
        }
        let (sandbox_mode, network_access) = effective_command_policy(profile);
        if audit.get("command_scope_digest").is_none() {
            audit["command_scope_digest"] = json!(command_script_scope_digest_with_policy(
                &input.command,
                input.effective_cwd(),
                input.effective_timeout_seconds(),
                sandbox_mode,
                network_access,
            ));
        }
        audit["approval_policy"] =
            serde_json::to_value(approval_policy).unwrap_or(json!("unknown"));
    } else {
        audit["argument_validation"] = json!("failed");
    }
    if audit.get("argument_validation").is_none() {
        match decision {
            ToolBrokerDecision::Allow => {
                audit["approval_decision"] = json!("allowed_by_policy");
            }
            ToolBrokerDecision::Approved { approval_grant_id } => {
                audit["approval_decision"] = json!("approved");
                audit["approval_grant_id"] = json!(approval_grant_id);
            }
            ToolBrokerDecision::Deny { reason, .. } => {
                audit["approval_decision"] = json!("denied");
                audit["approval_denial_reason"] = json!(reason);
            }
            ToolBrokerDecision::Ask {
                approval_request_id,
                reason,
            } => {
                audit["approval_decision"] = json!("approval_required");
                audit["approval_request_id"] = json!(approval_request_id);
                audit["approval_reason"] = json!(reason);
            }
        }
    }
    if audit.get("command_provenance").is_none() {
        audit["command_provenance"] = json!("agent_requested");
    }
    if audit.get("sandbox_backend").is_none() {
        audit["sandbox_backend"] = json!("not_executed");
    }
    audit
}

fn is_repairable_tool_result(tool_result: &ToolResult) -> bool {
    match tool_result.failure_kind.as_ref() {
        Some(
            ToolFailureKind::Input
            | ToolFailureKind::Visibility
            | ToolFailureKind::Capability
            | ToolFailureKind::Policy
            | ToolFailureKind::PermissionProfile
            | ToolFailureKind::WorkspaceBoundary
            | ToolFailureKind::ProtectedPath,
        ) => true,
        Some(ToolFailureKind::Execution) => tool_result
            .error_code
            .as_deref()
            .is_some_and(|error_code| REPAIRABLE_TOOL_ERROR_CODES.contains(&error_code)),
        _ => false,
    }
}

fn safe_plan_summary(plan: &AgentPlan) -> Value {
    serde_json::to_value(safe_agent_plan(plan)).expect("safe agent plan serializes")
}

fn safe_agent_plan(plan: &AgentPlan) -> AgentPlan {
    AgentPlan {
        steps: plan
            .steps
            .iter()
            .map(|plan_step| AgentPlanStep {
                step: safe_plan_step_text(&plan_step.step),
                status: plan_step.status,
            })
            .collect(),
    }
}

fn safe_plan_step_text(step: &str) -> String {
    if contains_sensitive_text(step) {
        return "[redacted plan step]".to_string();
    }
    step.chars().take(MAX_PLAN_STEP_CHARS).collect()
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key.clone(), canonical_json(value));
            }
            Value::Object(canonical)
        }
        value => value.clone(),
    }
}

fn tool_call_fingerprint(call: &ModelToolCall) -> String {
    let canonical_arguments = canonical_json(&call.arguments);
    let encoded = serde_json::to_vec(&(call.tool_name.as_str(), canonical_arguments))
        .expect("tool call fingerprint payload serializes");
    format!("sha256:{:x}", Sha256::digest(encoded))
}

fn repair_failure_signature(tool_call_fingerprint: &str, error_code: &str) -> String {
    let encoded = format!("{tool_call_fingerprint}\0{error_code}");
    format!("sha256:{:x}", Sha256::digest(encoded.as_bytes()))
}

fn is_sha256_fingerprint(value: &str) -> bool {
    let Some(value) = value.strip_prefix("sha256:") else {
        return false;
    };
    value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn approval_request(
    input: &AgentLoopInput,
    approval_request_id: &str,
    call: &ModelToolCall,
    reason: &str,
    profile: &PermissionProfile,
) -> ApprovalRequest {
    let mut request = ApprovalRequest::new(
        approval_request_id,
        input.thread_id.clone(),
        input.turn_id.clone(),
        call.tool_name.clone(),
    )
    .with_tool_call_id(call.tool_call_id.clone())
    .with_resources(permission_resources_for_tool(call, profile));
    request.reason = reason.to_string();
    request
}

fn approval_request_id(input: &AgentLoopInput, call: &ModelToolCall) -> String {
    approval_request_id_from_tool_call_id(&input.turn_id, &call.tool_call_id)
}

fn approval_request_id_from_tool_call_id(turn_id: &str, tool_call_id: &str) -> String {
    format!("approval_{}_{}", turn_id, tool_call_id)
}

#[derive(Debug, Error)]
enum CommandBindingError {
    #[error("{0}")]
    InvalidArguments(AgentLoopToolError),
}

fn canonicalize_model_tool_call(
    tool_broker: &ToolBroker,
    model_call: &ModelToolCall,
    permission_profile: &PermissionProfile,
) -> Result<ModelToolCall, String> {
    let (_, execution_arguments) = tool_broker
        .prepare_model_input(&model_call.tool_name, &model_call.arguments)
        .map_err(|error| error.code)?;
    let mut bound_call = model_call.clone();
    bound_call.raw_arguments = execution_arguments.to_string();
    bound_call.arguments = execution_arguments;
    let bound_call = bind_tool_call_to_profile(&bound_call, permission_profile)
        .map_err(|error| error.to_string())?;
    tool_broker
        .validate_execution_input(&bound_call.tool_name, &bound_call.arguments)
        .map_err(|error| error.code)?;
    Ok(bound_call)
}

fn bind_tool_call_to_profile(
    call: &ModelToolCall,
    _profile: &PermissionProfile,
) -> Result<ModelToolCall, CommandBindingError> {
    if call.tool_name != TOOL_COMMAND {
        return Ok(call.clone());
    }
    command_tool_input(&call.arguments).map_err(CommandBindingError::InvalidArguments)?;
    Ok(call.clone())
}

fn effective_command_policy(
    profile: &PermissionProfile,
) -> (SandboxFilesystemMode, SandboxNetworkMode) {
    let session_filesystem = match profile.profile {
        PermissionProfileName::ReadOnly => SandboxFilesystemMode::ReadOnly,
        PermissionProfileName::WorkspaceWrite => SandboxFilesystemMode::WorkspaceWrite,
    };
    let session_network = match profile.network_access {
        NetworkAccess::Denied => SandboxNetworkMode::Denied,
        NetworkAccess::Allowed => SandboxNetworkMode::Allowed,
    };
    (session_filesystem, session_network)
}

fn permission_operation_for_tool(tool_name: &str) -> PermissionOperation {
    match tool_name {
        TOOL_EDIT | TOOL_PATCH => PermissionOperation::Write,
        TOOL_COMMAND => PermissionOperation::Execute,
        _ => PermissionOperation::Read,
    }
}

fn permission_resources_for_tool(call: &ModelToolCall, profile: &PermissionProfile) -> Vec<String> {
    if call.tool_name == TOOL_COMMAND {
        return command_permission_resources(&call.arguments, &call.tool_name, profile);
    }
    let paths = path_arguments(&call.arguments);
    if paths.is_empty() {
        vec![call.tool_name.clone()]
    } else {
        paths
    }
}

fn command_permission_resources(
    arguments: &Value,
    fallback: &str,
    profile: &PermissionProfile,
) -> Vec<String> {
    let Ok(input) = serde_json::from_value::<CommandToolInput>(arguments.clone()) else {
        return vec![fallback.to_string()];
    };
    let (filesystem, network) = effective_command_policy(profile);
    let resource = command_script_scope_resource_with_policy(
        &input.command,
        input.effective_cwd(),
        input.effective_timeout_seconds(),
        filesystem,
        network,
    );
    if resource.is_empty() {
        vec![fallback.to_string()]
    } else {
        vec![resource]
    }
}

fn path_arguments(arguments: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        paths.push(path);
    }
    if let Some(changes) = arguments.get("changes").and_then(Value::as_array) {
        paths.extend(changes.iter().filter_map(|change| {
            change
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
        }));
    }
    paths
}

fn read_tool_input(arguments: &Value) -> Result<ReadToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn list_tool_input(arguments: &Value) -> Result<ListToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn grep_tool_input(arguments: &Value) -> Result<GrepToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn edit_tool_input(arguments: &Value) -> Result<EditToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn patch_tool_input(arguments: &Value) -> Result<WorkspacePatch, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn command_tool_input(arguments: &Value) -> Result<CommandToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn update_plan_tool_input(arguments: &Value) -> Result<AgentPlan, AgentLoopToolError> {
    let input: AgentPlanUpdateInput =
        serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)?;
    input
        .into_plan()
        .map_err(AgentLoopToolError::InvalidArguments)
}

fn permission_failure_kind(cause: &PermissionDecisionCause) -> ToolFailureKind {
    match cause {
        PermissionDecisionCause::FilesystemProfile => ToolFailureKind::PermissionProfile,
        PermissionDecisionCause::NetworkProfile => ToolFailureKind::PermissionProfile,
        PermissionDecisionCause::ProtectedResource => ToolFailureKind::ProtectedPath,
        PermissionDecisionCause::ApprovalPolicy => ToolFailureKind::Approval,
        PermissionDecisionCause::Explicit
        | PermissionDecisionCause::Rule
        | PermissionDecisionCause::NoMatchingRule => ToolFailureKind::Policy,
    }
}

fn validate_plan_tool_input_contract(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: AgentPlanUpdateInput = serde_json::from_value(input.clone())
        .map_err(|_| ToolInputValidationError::new("plan_input_shape_invalid"))?;
    AgentPlan { steps: input.steps }
        .validate_contract()
        .map_err(|failure| ToolInputValidationError::new(failure.code()))
}

fn invalid_tool_arguments_result(
    call: &ModelToolCall,
    error: ToolInputValidationError,
    spec: Option<&ToolSpec>,
) -> ToolResult {
    let envelope = tool_call_request(call);
    let mut audit = json!({
        "argument_validation": "failed",
        "argument_validation_code": &error.code,
        "policy_evaluated": false,
        "executor_started": false,
        "tool_provenance": "agent_requested",
    });
    if call.tool_name == TOOL_COMMAND {
        audit["sandbox_backend"] = json!("not_executed");
        audit["command_provenance"] = json!("agent_requested");
    }
    let output = if error.code == "tool_not_visible" {
        ToolOutput::failure_with_kind(
            ToolFailureKind::Visibility,
            "tool_not_visible",
            json!({
                "summary": "tool is not visible in the current model tool view",
                "validation_code": error.code,
            }),
        )
    } else if call.tool_name == TOOL_COMMAND {
        invalid_command_arguments_output(&error.code, spec)
    } else {
        ToolOutput::failure_with_kind(
            ToolFailureKind::Input,
            "invalid_tool_arguments",
            json!({
                "summary": "tool arguments failed executable input validation",
                "validation_code": error.code,
            }),
        )
    };
    ToolResult::from_result(&envelope, &output).with_audit(audit)
}

fn command_argument_validation_code(call: &ModelToolCall) -> &'static str {
    match call.arguments.get("command") {
        None => "missing_command",
        Some(Value::String(_)) => "invalid_command_arguments",
        Some(_) => "command_not_string",
    }
}

fn invalid_command_arguments_output(validation_code: &str, spec: Option<&ToolSpec>) -> ToolOutput {
    const MAX_SCHEMA_HINT_CHARS: usize = 2_048;
    // Serde's error text can include the offending scalar value. Keep the
    // validation code and schema hints useful to the model, but never echo
    // the raw argument payload through a public tool result.
    let mut summary = format!("invalid command arguments ({validation_code})");
    let retry_inputs = spec.map(ToolSpec::exact_model_inputs).unwrap_or_default();
    if !retry_inputs.is_empty() {
        summary.push_str(
            ". The command field must be a shell command string. Copy one complete retry_inputs object exactly",
        );
    }
    if let Some(schema) = spec.map(|spec| &spec.input_schema) {
        let schema = schema.to_string();
        let mut hint = schema
            .chars()
            .take(MAX_SCHEMA_HINT_CHARS)
            .collect::<String>();
        if schema.chars().count() > hint.chars().count() {
            hint.push_str("...");
        }
        summary.push_str(". Retry with one exact JSON input allowed by this schema: ");
        summary.push_str(&hint);
    }
    ToolOutput::failure_with_kind(
        ToolFailureKind::Input,
        "invalid_tool_arguments",
        json!({
            "summary": summary,
            "validation_code": validation_code,
            "retry_inputs": retry_inputs,
        }),
    )
}

fn invalid_tool_arguments(error: serde_json::Error) -> AgentLoopToolError {
    AgentLoopToolError::InvalidArguments(error.to_string())
}

fn cancelled_tool_result(call: &ModelToolCall) -> ToolResult {
    ToolResult::failed_with_kind(
        &tool_call_request(call),
        ToolFailureKind::Cancelled,
        "tool_cancelled",
        "tool execution cancelled",
    )
}

fn workspace_tool_failure(error: AgentLoopToolError) -> ToolOutput {
    let (failure_kind, error_code) = match &error {
        AgentLoopToolError::InvalidArguments(_) => {
            (ToolFailureKind::Input, "invalid_tool_arguments")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::OutsideWorkspace(_)) => {
            (ToolFailureKind::WorkspaceBoundary, "outside_workspace")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ProtectedPath(_)) => {
            (ToolFailureKind::ProtectedPath, "protected_path")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::SandboxUnavailable) => {
            (ToolFailureKind::Sandbox, "sandbox_unavailable")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::Cancelled) => {
            (ToolFailureKind::Cancelled, "tool_cancelled")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::BinaryPattern) => {
            (ToolFailureKind::Execution, "binary_pattern")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ReadFailed(_)) => {
            (ToolFailureKind::Execution, "tool_read_failed")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::RollbackFailed(_)) => {
            (ToolFailureKind::Infrastructure, "workspace_rollback_failed")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ExpectedContentMissing(_)) => {
            (ToolFailureKind::Execution, "expected_content_missing")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::InvalidInput(_)) => {
            (ToolFailureKind::Input, "invalid_tool_input")
        }
    };
    let summary = match error {
        // Serde's error text may contain a value copied from the model's raw
        // arguments. The model still receives the stable error code and can
        // consult the registered schema; it must not receive that payload
        // echoed back through the public result.
        AgentLoopToolError::InvalidArguments(_) => "tool arguments failed schema validation",
        AgentLoopToolError::Workspace(WorkspaceToolError::OutsideWorkspace(_)) => {
            "tool path is outside the workspace"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ProtectedPath(_)) => {
            "tool path is protected"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::SandboxUnavailable) => {
            "strict sandbox backend is unavailable"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::Cancelled) => "tool execution cancelled",
        AgentLoopToolError::Workspace(WorkspaceToolError::RollbackFailed(_)) => {
            "workspace rollback failed"
        }
        _ => {
            return ToolOutput::failure_with_kind(
                failure_kind,
                error_code,
                json!({"summary": error.to_string()}),
            );
        }
    };
    ToolOutput::failure_with_kind(failure_kind, error_code, json!({"summary": summary}))
}

fn command_workspace_tool_failure(
    input: &CommandToolInput,
    error: AgentLoopToolError,
    profile: &PermissionProfile,
) -> ToolOutput {
    let mut output = workspace_tool_failure(error);
    let (sandbox_mode, network_access) = effective_command_policy(profile);
    output.metadata["audit"] = json!({
        "cwd": input.effective_cwd(),
        "timeout_seconds": input.effective_timeout_seconds(),
        "sandbox_mode": sandbox_mode,
        "network_access": network_access,
        "sandbox_backend": "unavailable",
        "sandbox_enforcement": "unavailable",
        "command_scope_digest": command_script_scope_digest_with_policy(
            &input.command,
            input.effective_cwd(),
            input.effective_timeout_seconds(),
            sandbox_mode,
            network_access,
        ),
        "command_provenance": "agent_requested",
    });
    output
}

#[cfg(test)]
mod scheduler_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::parallel_map;

    #[test]
    fn parallel_map_overlaps_workers_and_preserves_input_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));
        let results = parallel_map(vec![0usize, 1usize], {
            let active = Arc::clone(&active);
            let overlapped = Arc::clone(&overlapped);
            move |index| {
                active.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(1);
                while active.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                if active.load(Ordering::SeqCst) == 2 {
                    overlapped.store(true, Ordering::SeqCst);
                }
                (index, overlapped.load(Ordering::SeqCst))
            }
        });

        assert_eq!(results, vec![Some((0, true)), Some((1, true))]);
    }
}

#[cfg(test)]
mod audit_projection_tests {
    use super::*;

    #[test]
    fn audit_projection_is_a_closed_typed_allowlist() {
        let raw = json!({
            "cwd": "C:/sensitive/workspace",
            "raw_arguments": {"command": "echo secret"},
            "approval_reason": "operator reason must not escape",
            "approval_request_id": "approval-secret",
            "approval_grant_id": "grant-secret",
            "extra": {"nested": "must not escape"},
            "sandbox_mode": "workspace_write",
            "network_access": "allowed",
            "sandbox_backend": "test_backend",
            "sandbox_enforcement": "strict",
            "local_process_fallback": false,
            "command_scope_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "command_provenance": "agent_requested",
            "approval_policy": "on-request",
            "approval_decision": "approved",
            "timeout_seconds": 5,
        });

        let projected = project_audit_event(&raw);
        assert_eq!(projected["sandbox_mode"], "workspace_write");
        assert_eq!(
            projected["command_scope_digest"],
            raw["command_scope_digest"]
        );
        assert!(projected.get("cwd").is_none());
        let serialized = serde_json::to_string(&projected).expect("serialize audit projection");
        for forbidden in [
            "raw_arguments",
            "sensitive/workspace",
            "approval-secret",
            "grant-secret",
            "operator reason",
            "must not escape",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn eligible_command_observations_filter_failures_and_pre_mutation_results() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut failed = ToolResult::summary("failed", TOOL_COMMAND, false, "failed");
        failed.result_id = Some(digest.to_string());
        let mut before_mutation = ToolResult::summary("before", TOOL_COMMAND, true, "ok");
        before_mutation.result_id = Some(digest.to_string());
        let mutation = ToolResult::summary("edit", TOOL_EDIT, true, "changed");
        let mut after_mutation = ToolResult::summary("after", TOOL_COMMAND, true, "ok");
        after_mutation.result_id = Some(digest.to_string());

        assert_eq!(
            eligible_command_scope_digests(&[failed, before_mutation, mutation, after_mutation,]),
            vec![digest.to_string()]
        );
    }

    #[test]
    fn completion_gate_requires_exact_successful_command_observation_after_mutation() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut tracker = CompletionTracker::default();
        tracker.observe(&ToolResult::summary("edit", TOOL_EDIT, true, "changed"));

        let mut failed = ToolResult::summary("failed", TOOL_COMMAND, false, "failed");
        failed.result_id = Some(digest.to_string());
        tracker.observe(&failed);
        assert!(!tracker.verification_satisfied());

        let mut malformed = ToolResult::summary("malformed", TOOL_COMMAND, true, "ok");
        malformed.result_id = Some("sha256:not-a-digest".to_string());
        tracker.observe(&malformed);
        assert!(!tracker.verification_satisfied());

        let mut successful = ToolResult::summary("successful", TOOL_COMMAND, true, "ok");
        successful.result_id = Some(digest.to_string());
        tracker.observe(&successful);
        assert!(tracker.verification_satisfied());
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn workspace_cancellation_maps_to_stable_tool_result() {
        let output =
            workspace_tool_failure(AgentLoopToolError::Workspace(WorkspaceToolError::Cancelled));
        assert!(!output.ok);
        assert_eq!(output.error_code.as_deref(), Some("tool_cancelled"));
        assert_eq!(output.failure_kind, Some(ToolFailureKind::Cancelled));

        let envelope = ToolCallRequest::new("call_1", TOOL_READ, "{}");
        let result = ToolResult::from_result(&envelope, &output);
        assert!(!result.ok);
        assert_eq!(result.error_code.as_deref(), Some("tool_cancelled"));
        assert_eq!(result.failure_kind, Some(ToolFailureKind::Cancelled));
        assert_eq!(result.to_message_payload()["ok"], false);
    }

    #[test]
    fn late_success_is_replaced_by_cancellation_result() {
        let call = ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_READ.to_string(),
            raw_arguments: "{}".to_string(),
            arguments: json!({}),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        };

        let result = cancelled_tool_result(&call);
        assert!(!result.ok);
        assert_eq!(result.error_code.as_deref(), Some("tool_cancelled"));
        assert_eq!(result.failure_kind, Some(ToolFailureKind::Cancelled));
    }
}
