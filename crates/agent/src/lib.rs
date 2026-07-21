#![forbid(unsafe_code)]

//! 负责模型 turn、tool 执行、approval 检查点和完成校验的 `AgentLoop` 状态机。
//!
//! loop 将模型提供方可见历史与规范化可执行调用分离，所有副作用都经由 `ToolBroker`，
//! 并在完成或恢复不变量不满足时拒绝继续执行。

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::ControlFlow;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, ProjectInstructions, contains_sensitive_text};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelError, ModelErrorCategory,
    ModelErrorKind, ModelMessage, ModelPreferences, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelToolSchema, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptEvent, ProviderAttemptMetadata,
    ProviderAttemptStarted, ProviderCapabilityMetadata, ProviderDiagnostic, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderStreamEvent, ToolChoiceMode,
    is_strict_tool_schema_compatible, provider_error_response,
    validate_model_request_with_capabilities, validate_model_turn_response,
};
use singularity_policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, NetworkAccess, PermissionDecision,
    PermissionDecisionCause as PermissionCause, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionProfileName, PermissionRequest, PermissionResource, PolicyEngine,
    ToolId,
};
use singularity_tools::{
    AgentControlToolExecutor, BoundToolCall, COMMAND_TOOL as TOOL_COMMAND, CommandToolInput,
    EDIT_TOOL as TOOL_EDIT, EditToolInput, GrepToolInput, ListToolInput, PATCH_TOOL as TOOL_PATCH,
    ReadToolInput, SandboxExecutionBoundary as ToolSandboxExecutionBoundary,
    SandboxExecutionObservation as ToolSandboxExecutionObservation,
    SandboxExecutionSinkError as ToolSandboxExecutionSinkError,
    SandboxExecutionStatus as ToolSandboxExecutionStatus, SandboxFilesystemMode,
    SandboxNetworkMode, ToolAuthorization, ToolBroker, ToolBrokerDecision, ToolCallRequest,
    ToolCapability, ToolEntry, ToolExecutionMode, ToolExecutor, ToolFailureKind,
    ToolInputValidationError, ToolOutput, ToolResult, ToolSpec, WorkspacePatch, WorkspaceToolError,
    WorkspaceToolExecutor, WorkspaceTools, approximate_token_count,
    command_script_scope_digest_with_policy,
};
use thiserror::Error;

mod checkpoint;
mod completion;
mod context;
mod observation;

pub use checkpoint::{ApprovalCheckpoint, PendingApprovalOccurrence, PendingToolCall};
use completion::{
    CompletionTracker, RepairFailureState, ToolResultOccurrence, ToolResultVisibility,
};
pub use completion::{successful_command_scope_digest, terminal_command_scope_digests};
pub use context::{
    AgentContextItem, AgentContextItemPriority, AgentContextTrace, ContextBundle,
    assemble_context_items,
};
use context::{
    ContextBudget, assemble_context_items_with_budget, current_turn_excluded,
    model_messages_from_context,
};
use observation::OccurrenceTimer;
pub use observation::{
    AgentLoopEvent, AgentLoopEventCallback, AgentLoopEventSinkError, AgentObservation,
    FinalReviewObservation, FinalReviewStatus, OccurrenceIdentity, OccurrenceLifecycle,
    PolicyDecisionCause, PolicyDecisionObservation, PolicyDecisionStatus,
    PromptAssemblyObservation, PromptAssemblyStatus, ProviderAttemptObservation,
    ProviderAttemptStatus, ProviderAttemptUsageObservation, SandboxExecutionOccurrence,
    SandboxExecutionStatus, ToolCallObservation, ToolCallStatus, VerificationObservation,
    VerificationStatus,
};

#[cfg(test)]
use completion::ToolResultOccurrenceWire;
#[cfg(test)]
use singularity_tools::{WorkspaceObservation, WorkspaceRevision};

const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 16;
const MAX_PARALLEL_READ_TOOL_CALLS: u32 = 8;
const APPROVAL_CHECKPOINT_VERSION: u32 = 2;
const AGENT_DEVELOPER_INSTRUCTIONS: &str = "You are a coding agent working in the current workspace. Inspect real files before making claims. Use tools for changes, write only inside the workspace, and run verification after the last mutation. Report only completed work and verification. Read-only questions need no changes or verification. For multi-step work, keep a concise update_plan plan; revise it when evidence or failure changes the approach, and complete it before the final answer. Skip plans for simple read-only or single-step work. Tools can be submitted only through native structured tool calls; ordinary text is never executed. Match registered tool schemas exactly and use typed tool results to correct parameters.";
const USER_MESSAGE_ROLE: &str = "user";
const ASSISTANT_MESSAGE_ROLE: &str = "assistant";
const MODEL_MESSAGE_FRAMING_TOKENS: u32 = 4;
const MODEL_REQUEST_FIXED_OVERHEAD_TOKENS: u32 = 256;
const COMPACTED_TOOL_RESULT_INSTRUCTION: &str = "The prior tool output was omitted to fit the context window. Re-read the relevant file or rerun a safe command if exact output is needed.";
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
const EVENT_SINK_FAILURE_ERROR: &str = "agent event sink failed";

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

/// 返回用于更新计划的独占控制 tool 注册表条目。
pub fn agent_control_tool_entries() -> Vec<ToolEntry> {
    let spec = ToolSpec::new(
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
    );
    vec![
        ToolEntry::model(
            spec,
            1,
            ToolCapability::PlanManagement,
            ToolAuthorization::AgentControl,
            ToolExecutor::AgentControl(AgentControlToolExecutor::UpdatePlan),
        )
        .expect("built-in agent control tool entry is valid"),
    ]
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
    project_instructions: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    project_instructions_digest: Option<String>,
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
            project_instructions_digest: None,
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

    /// Binds a verified project-instruction aggregate to this turn.
    pub fn with_project_instructions(mut self, instructions: ProjectInstructions) -> Self {
        let (content, aggregate_digest) = instructions.into_snapshot();
        self.project_instructions = Some(content);
        self.project_instructions_digest = Some(aggregate_digest);
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
    pub tool_name: ToolId,
    pub resources: Vec<PermissionResource>,
    pub outcome: ApprovalOutcome,
}

impl ApprovalGrant {
    /// 构造允许恢复的 approval 授权。
    pub fn allow<I>(request_id: impl Into<String>, tool_name: ToolId, resources: I) -> Self
    where
        I: IntoIterator<Item = PermissionResource>,
    {
        Self {
            request_id: request_id.into(),
            tool_name,
            resources: resources.into_iter().collect(),
            outcome: ApprovalOutcome::Allow,
        }
    }
}

/// 一次运行的完整结果，包括按 occurrence 顺序保留的待处理 approval 和 tool 结果。
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct AgentLoopResult {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    #[serde(
        rename = "approval_requests",
        serialize_with = "serialize_pending_approval_requests"
    )]
    #[schemars(skip)]
    /// Each entry owns its request and the typed checkpoint for the executable call.
    pub pending_approvals: Vec<PendingApprovalOccurrence>,
    /// Public projection of the ordered internal tool-result occurrences.
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

fn serialize_pending_approval_requests<S>(
    pending_approvals: &[PendingApprovalOccurrence],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    pending_approvals
        .iter()
        .map(PendingApprovalOccurrence::request)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

impl AgentLoopResult {
    /// 按 request id 查找一个完整的 typed approval occurrence。
    pub fn pending_approval(&self, request_id: &str) -> Option<&PendingApprovalOccurrence> {
        self.pending_approvals
            .iter()
            .find(|occurrence| occurrence.request().request_id == request_id)
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

/// 在形成 `AgentLoopResult` 前跨模型提供方 turn 累积的可变状态。
struct AgentLoopState {
    messages: Vec<ModelMessage>,
    tool_result_occurrences: Vec<ToolResultOccurrence>,
    pending_approvals: Vec<PendingApprovalOccurrence>,
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
            tool_result_occurrences: Vec::new(),
            pending_approvals: Vec::new(),
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
            .saturating_add(self.pending_approvals.len() as u32);
        let public_plan = self.plan.as_ref().map(safe_agent_plan);
        let tool_results = self
            .tool_result_occurrences
            .into_iter()
            .map(ToolResultOccurrence::into_result)
            .collect::<Vec<_>>();
        AgentLoopResult {
            status,
            completed,
            final_answer,
            model_turns,
            tool_calls: tool_results.len() as u32,
            approval_count,
            pending_approvals: self.pending_approvals,
            tool_results,
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
        model_turn_ordinal: u32,
        contract: &ProviderProtocolContract,
        metadata: &ProviderCapabilityMetadata,
    ) {
        self.provider_protocol_contract = Some(contract.clone());
        self.record_provider_capability_metadata(metadata, model_turn_ordinal, None);
    }

    fn record_provider_negotiation_error(
        &mut self,
        model_turn_ordinal: u32,
        error: &ProviderError,
    ) {
        self.provider_protocol_contract = None;
        if let Some(metadata) = error.capability_metadata.as_deref() {
            self.record_provider_capability_metadata(metadata, model_turn_ordinal, None);
        }
    }

    fn approval_count(&self) -> u32 {
        self.prior_approval_count
            .saturating_add(self.pending_approvals.len() as u32)
    }

    fn observe_model_response(
        &mut self,
        response: &ModelTurnResponse,
        model_turn_ordinal: u32,
        parent_occurrence_id: &str,
    ) {
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
            let first_attempt_index = self.provider_attempts.attempt_count.saturating_add(1);
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
            self.provider_attempts.occurrences.extend(
                metadata
                    .occurrences
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(offset, mut occurrence)| {
                        occurrence.attempt_index = first_attempt_index
                            .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
                        occurrence.model_turn_ordinal = Some(model_turn_ordinal);
                        occurrence.parent_occurrence_id = Some(parent_occurrence_id.to_string());
                        occurrence
                    }),
            );
        }
        if let Some(metadata) = &response.provider_capability_metadata {
            self.record_provider_capability_metadata(
                metadata,
                model_turn_ordinal,
                Some(parent_occurrence_id),
            );
        }
    }

    fn record_provider_capability_metadata(
        &mut self,
        metadata: &ProviderCapabilityMetadata,
        model_turn_ordinal: u32,
        parent_occurrence_id: Option<&str>,
    ) {
        let mut metadata = metadata.clone();
        let parent_occurrence_id = parent_occurrence_id.map(str::to_string);
        for observation in &mut metadata.cache_observations {
            observation.model_turn_ordinal = Some(model_turn_ordinal);
            observation.parent_occurrence_id = parent_occurrence_id.clone();
        }
        for occurrence in &mut metadata.probe_attempt_metadata.occurrences {
            occurrence.model_turn_ordinal = Some(model_turn_ordinal);
            occurrence.parent_occurrence_id = parent_occurrence_id.clone();
        }
        self.provider_capability_metadata = Some(match self.provider_capability_metadata.take() {
            Some(previous) => merge_provider_capability_metadata(previous, metadata),
            None => metadata,
        });
    }

    /// Bind pre-request negotiation evidence only after its real PromptAssembly start is emitted.
    fn bind_unbound_provider_runtime_observations(
        &mut self,
        model_turn_ordinal: u32,
        parent_occurrence_id: &str,
    ) {
        let Some(metadata) = &mut self.provider_capability_metadata else {
            return;
        };
        let parent_occurrence_id = parent_occurrence_id.to_string();
        for observation in &mut metadata.cache_observations {
            if observation.parent_occurrence_id.is_none() {
                observation.model_turn_ordinal = Some(model_turn_ordinal);
                observation.parent_occurrence_id = Some(parent_occurrence_id.clone());
            }
        }
        for occurrence in &mut metadata.probe_attempt_metadata.occurrences {
            if occurrence.parent_occurrence_id.is_none() {
                occurrence.model_turn_ordinal = Some(model_turn_ordinal);
                occurrence.parent_occurrence_id = Some(parent_occurrence_id.clone());
            }
        }
    }

    fn checkpoint(
        &self,
        input: &AgentLoopInput,
        pending_tool_call: &PendingToolCall,
        model_turns: u32,
    ) -> Result<ApprovalCheckpoint, String> {
        // Runtime provider occurrences are delivery-scoped; checkpoint only carries the
        // aggregate counters so a decoded resume emits new observations exactly once.
        let mut provider_attempts = self.provider_attempts.clone();
        provider_attempts.occurrences.clear();
        let checkpoint = ApprovalCheckpoint {
            pending_tool_call: pending_tool_call.clone(),
            checkpoint_version: APPROVAL_CHECKPOINT_VERSION,
            thread_id: input.thread_id.clone(),
            turn_id: input.turn_id.clone(),
            project_instructions_digest: input.project_instructions_digest.clone(),
            messages: self.messages.clone(),
            tool_result_occurrences: self.tool_result_occurrences.clone(),
            used_approval_grants: self.used_approval_grants.iter().cloned().collect(),
            approval_count: self.approval_count().saturating_add(1),
            model_turns,
            completion: self.completion.clone(),
            last_completion_error: self.last_completion_error.clone(),
            plan: self.plan.clone(),
            plan_update_count: self.plan_update_count,
            recovery_metrics: self.recovery_metrics.clone(),
            model_usage: self.model_usage.clone(),
            provider_attempts,
            context_trace: self.context_trace.clone(),
            seen_tool_call_fingerprints: self.seen_tool_call_fingerprints.iter().cloned().collect(),
            last_repair_failure: self.last_repair_failure.clone(),
        };
        checkpoint.validate_serialized()?;
        Ok(checkpoint)
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

    fn append_visible_tool_result(
        &mut self,
        tool_result: ToolResult,
        provider_tool_name: Option<&str>,
    ) {
        self.messages
            .push(tool_result_message(&tool_result, provider_tool_name));
        self.tool_result_occurrences.push(ToolResultOccurrence::new(
            tool_result,
            ToolResultVisibility::Visible,
        ));
    }

    fn append_hidden_tool_result(&mut self, tool_result: ToolResult) {
        self.tool_result_occurrences.push(ToolResultOccurrence::new(
            tool_result,
            ToolResultVisibility::Hidden,
        ));
    }
}

#[derive(Clone)]
struct PreparedToolCall {
    call: ModelToolCall,
    fingerprint: String,
    bound: Option<BoundToolCall>,
    decision: Option<ToolBrokerDecision>,
    rejection: Option<ToolResult>,
}

enum ToolBatchControl {
    Continue,
    Blocked,
    Failed(String),
    Cancelled,
}

/// 调用方消费最终化 assistant 文本 delta 的窄 callback 类型。
pub type AgentTextDeltaCallback<'a> = dyn FnMut(&str) + 'a;

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
        self.run_internal(input, None)
    }

    /// 运行一个新 turn，并在真实边界向调用方投影有序 typed 事件。
    pub fn run_with_events(
        &self,
        input: &AgentLoopInput,
        on_event: &mut AgentLoopEventCallback<'_>,
    ) -> AgentLoopResult {
        self.run_internal(input, Some(on_event))
    }

    /// 运行一个新 turn，并只向调用方投影最终化 assistant 回合的有序文本 delta。
    pub fn run_with_text_deltas(
        &self,
        input: &AgentLoopInput,
        on_text_delta: &mut AgentTextDeltaCallback<'_>,
    ) -> AgentLoopResult {
        self.run_internal(
            input,
            Some(&mut |event| {
                if let AgentLoopEvent::FinalTextDelta { delta } = event {
                    on_text_delta(&delta);
                }
                Ok(())
            }),
        )
    }

    fn run_internal(
        &self,
        input: &AgentLoopInput,
        mut on_event: Option<&mut AgentLoopEventCallback<'_>>,
    ) -> AgentLoopResult {
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
        let (capabilities, mut state) =
            match self.negotiate_tool_capabilities(input, state, 0, &mut on_event) {
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
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            0,
            on_event,
        )
    }

    /// 在每次模型提供方响应或 tool 结果后推进状态机。
    #[allow(clippy::too_many_arguments)]
    fn continue_run(
        &self,
        input: &AgentLoopInput,
        budget: &ContextBudget,
        capabilities: &ProviderProtocolContract,
        max_tool_calls: u32,
        mut state: AgentLoopState,
        model_turn_offset: u32,
        on_event: Option<&mut AgentLoopEventCallback<'_>>,
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
        let mut on_event = on_event;
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
                let gate_timer = OccurrenceTimer::start();
                let gate_identity = occurrence_identity(
                    input,
                    "verification_gate",
                    turn_index,
                    state.recovery_metrics.completion_rejection_count,
                    None,
                );
                let summary = state.completion.summary();
                for lifecycle in [
                    gate_timer.started(),
                    gate_timer.finished(VerificationStatus::GatePassed),
                ] {
                    if emit_event(
                        &mut on_event,
                        AgentLoopEvent::Observation(AgentObservation::Verification(
                            VerificationObservation {
                                identity: gate_identity.clone(),
                                lifecycle,
                                required_command_count: summary.required_command_count,
                                satisfied_command_count: summary.satisfied_command_count,
                                occurrence_count: state.recovery_metrics.completion_rejection_count,
                                command_duration_ms: None,
                            },
                        )),
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            turn_index,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
                }
            }
            let prompt_identity =
                occurrence_identity(input, "prompt_assembly", turn_index, 0, None);
            let prompt_timer = OccurrenceTimer::start();
            if emit_event(
                &mut on_event,
                AgentLoopEvent::Observation(AgentObservation::PromptAssembly(
                    PromptAssemblyObservation {
                        identity: prompt_identity.clone(),
                        lifecycle: prompt_timer.started(),
                        model_turn_ordinal: turn_index,
                        message_count: 0,
                        tool_count: 0,
                        request_token_count: 0,
                        request_digest: String::new(),
                        compacted: false,
                        finalization_only,
                    },
                )),
            )
            .is_err()
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
            state.bind_unbound_provider_runtime_observations(
                turn_index,
                &prompt_identity.occurrence_id,
            );
            let tool_view = if finalization_only {
                ModelToolView::finalization()
            } else {
                match model_tool_view(&self.tool_broker, capabilities, max_tool_calls) {
                    Ok(tool_view) => tool_view,
                    Err(error) => {
                        if emit_prompt_assembly_finished(
                            &mut on_event,
                            prompt_identity,
                            &prompt_timer,
                            turn_index,
                            0,
                            0,
                            0,
                            String::new(),
                            false,
                            finalization_only,
                            PromptAssemblyStatus::ToolViewRejected,
                        )
                        .is_err()
                        {
                            return state.finish(
                                AgentStatus::Failed,
                                false,
                                None,
                                turn_index,
                                Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                            );
                        }
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
            let mut compacted = false;
            if !model_request_fits_context(
                &tool_view.tools,
                &state.messages,
                &state.tool_result_occurrences,
                budget,
            ) {
                let Some(compaction) = compact_model_messages(&tool_view.tools, &state, budget)
                else {
                    let message_count = u32::try_from(state.messages.len()).unwrap_or(u32::MAX);
                    let tool_count = u32::try_from(tool_view.tools.len()).unwrap_or(u32::MAX);
                    let request_token_count = model_request_token_count(
                        &tool_view.tools,
                        &state.messages,
                        &state.tool_result_occurrences,
                        budget,
                    );
                    if emit_prompt_assembly_finished(
                        &mut on_event,
                        prompt_identity,
                        &prompt_timer,
                        turn_index,
                        message_count,
                        tool_count,
                        request_token_count,
                        String::new(),
                        false,
                        finalization_only,
                        PromptAssemblyStatus::ContextOverflow,
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            turn_index,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        turn_index,
                        Some(MODEL_REQUEST_CONTEXT_OVERFLOW_ERROR.to_string()),
                    );
                };
                compacted = true;
                if let Some(context_trace) = &mut state.context_trace {
                    context_trace.record_compaction(&compaction);
                }
                state.messages = compaction.messages;
                state.tool_result_occurrences = compaction.tool_result_occurrences;
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
                let request_digest = safe_request_digest(&request);
                let message_count = u32::try_from(request.messages.len()).unwrap_or(u32::MAX);
                let tool_count = u32::try_from(request.tools.len()).unwrap_or(u32::MAX);
                let request_token_count = model_request_token_count(
                    &request.tools,
                    &request.messages,
                    &state.tool_result_occurrences,
                    budget,
                );
                if emit_prompt_assembly_finished(
                    &mut on_event,
                    prompt_identity,
                    &prompt_timer,
                    turn_index,
                    message_count,
                    tool_count,
                    request_token_count,
                    request_digest,
                    compacted,
                    finalization_only,
                    PromptAssemblyStatus::ValidationFailed,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        turn_index,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
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
            let request_digest = safe_request_digest(&request);
            let message_count = u32::try_from(request.messages.len()).unwrap_or(u32::MAX);
            let tool_count = u32::try_from(request.tools.len()).unwrap_or(u32::MAX);
            let request_token_count = model_request_token_count(
                &request.tools,
                &request.messages,
                &state.tool_result_occurrences,
                budget,
            );
            if emit_prompt_assembly_finished(
                &mut on_event,
                prompt_identity.clone(),
                &prompt_timer,
                turn_index,
                message_count,
                tool_count,
                request_token_count,
                request_digest,
                compacted,
                finalization_only,
                PromptAssemblyStatus::Ready,
            )
            .is_err()
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
            let final_review = finalization_only.then(|| {
                (
                    child_occurrence_identity(&prompt_identity, "final_review", 0),
                    OccurrenceTimer::start(),
                )
            });
            if let Some((identity, timer)) = &final_review
                && emit_event(
                    &mut on_event,
                    AgentLoopEvent::Observation(AgentObservation::FinalReview(
                        FinalReviewObservation {
                            identity: identity.clone(),
                            lifecycle: timer.started(),
                            model_turn_ordinal: turn_index,
                        },
                    )),
                )
                .is_err()
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
            let provider_events = RefCell::new(ProviderEventBridge::new(
                prompt_identity.clone(),
                finalization_only,
                &mut on_event,
            ));
            let stream_result = {
                let mut on_stream = |event| provider_events.borrow_mut().on_stream(event);
                let mut on_attempt = |event| provider_events.borrow_mut().on_attempt(event);
                self.provider.complete_stream_observed(
                    &request,
                    &self.cancellation,
                    &mut on_stream,
                    &mut on_attempt,
                )
            };
            let response = match stream_result {
                Ok(response) if response.status == ModelTurnStatus::Success => {
                    let terminal_text = assistant_message_text(response.assistant_message.as_ref());
                    if provider_events.borrow().streamed_text != terminal_text {
                        provider_error_model_response(
                            &request,
                            ProviderError::from_model_error(provider_stream_text_mismatch_error()),
                        )
                    } else {
                        response
                    }
                }
                Ok(response) => response,
                Err(error)
                    if error.error.code.as_deref() == Some(PROVIDER_STREAMING_UNSUPPORTED_CODE) =>
                {
                    let stream_error = error;
                    let completion = {
                        let mut on_attempt = |event| provider_events.borrow_mut().on_attempt(event);
                        self.provider.complete_observed(
                            &request,
                            &self.cancellation,
                            &mut on_attempt,
                        )
                    };
                    match completion {
                        Ok(mut response) => {
                            merge_response_runtime_metadata(&mut response, &stream_error);
                            response
                        }
                        Err(mut error) => {
                            merge_provider_error_runtime_metadata(&mut error, &stream_error);
                            provider_error_model_response(&request, error)
                        }
                    }
                }
                Err(error) => provider_error_model_response(&request, error),
            };
            let provider_events = provider_events.into_inner();
            if provider_events.event_sink_failed || provider_events.active_attempt.is_some() {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
            state.observe_model_response(&response, turn_index, &prompt_identity.occurrence_id);
            actual_model_turns = turn_index.saturating_add(1);
            if self.is_cancelled(input) {
                if emit_final_review_finished(
                    &mut on_event,
                    &final_review,
                    turn_index,
                    FinalReviewStatus::Cancelled,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
                return state.finish(
                    AgentStatus::Cancelled,
                    false,
                    None,
                    actual_model_turns,
                    None,
                );
            }
            if response.status != ModelTurnStatus::Success {
                if emit_final_review_finished(
                    &mut on_event,
                    &final_review,
                    turn_index,
                    FinalReviewStatus::Failed,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
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
                if emit_final_review_finished(
                    &mut on_event,
                    &final_review,
                    turn_index,
                    FinalReviewStatus::Failed,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
                for call in &response.tool_calls {
                    state.observe_model_tool_call(call, &provider_tool_names);
                }
                if emit_rejected_tool_calls(&mut on_event, input, &response.tool_calls, turn_index)
                    .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
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
                if emit_final_review_finished(
                    &mut on_event,
                    &final_review,
                    turn_index,
                    FinalReviewStatus::Failed,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
                if emit_rejected_tool_calls(&mut on_event, input, &response.tool_calls, turn_index)
                    .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
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
            if has_duplicate_tool_call_ids(&response.tool_calls) {
                if emit_final_review_finished(
                    &mut on_event,
                    &final_review,
                    turn_index,
                    FinalReviewStatus::Failed,
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
                if emit_rejected_tool_calls(&mut on_event, input, &response.tool_calls, turn_index)
                    .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
                let model_error =
                    model_response_validation_error(vec!["duplicate_tool_call_id".to_string()]);
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
                    if emit_final_review_finished(
                        &mut on_event,
                        &final_review,
                        turn_index,
                        FinalReviewStatus::Failed,
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            actual_model_turns,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
                    state.recovery_metrics.completion_rejection_count = state
                        .recovery_metrics
                        .completion_rejection_count
                        .saturating_add(1);
                    let rejection_count = state.recovery_metrics.completion_rejection_count;
                    if emit_verification_occurrence(
                        &mut on_event,
                        input,
                        turn_index,
                        rejection_count,
                        "verification_gate_rejected",
                        VerificationStatus::GateRejected,
                        &state.completion.summary(),
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            actual_model_turns,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    );
                }
                if state.allows_final() {
                    if emit_final_review_finished(
                        &mut on_event,
                        &final_review,
                        turn_index,
                        FinalReviewStatus::Succeeded,
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            actual_model_turns,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
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
                let rejection_count = state.recovery_metrics.completion_rejection_count;
                let verification = state.completion.summary();
                for (kind, status) in [
                    (
                        "verification_gate_rejected",
                        VerificationStatus::GateRejected,
                    ),
                    (
                        "verification_repair_requested",
                        VerificationStatus::RepairRequested,
                    ),
                ] {
                    if emit_verification_occurrence(
                        &mut on_event,
                        input,
                        turn_index,
                        rejection_count,
                        kind,
                        status,
                        &verification,
                    )
                    .is_err()
                    {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            actual_model_turns,
                            Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                        );
                    }
                }
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
            let mut tool_occurrences = execution_tool_calls
                .iter()
                .enumerate()
                .map(|(ordinal, call)| ModelToolOccurrence {
                    call: call.clone(),
                    fingerprint: String::new(),
                    invalid_was_observed: false,
                    context: tool_occurrence_context(
                        input,
                        call,
                        turn_index,
                        u32::try_from(ordinal).unwrap_or(u32::MAX),
                    ),
                })
                .collect::<Vec<_>>();
            for occurrence in &tool_occurrences {
                if emit_event(
                    &mut on_event,
                    tool_call_event(&occurrence.context, occurrence.context.timer.started()),
                )
                .is_err()
                {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                    );
                }
            }
            for occurrence in &mut tool_occurrences {
                let (fingerprint, invalid_was_observed) =
                    state.observe_model_tool_call(&occurrence.call, &execution_tool_names);
                occurrence.fingerprint = fingerprint;
                occurrence.invalid_was_observed = invalid_was_observed;
            }
            let assistant_tool_message = provider_history_assistant_message(
                response.assistant_message.as_ref(),
                &response.tool_calls,
                &execution_tool_calls,
            );
            state.messages.push(assistant_tool_message);
            match self.process_tool_calls(
                input,
                &tool_occurrences,
                &mut state,
                actual_model_turns,
                &mut on_event,
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

    /// 恢复一个已校验的 typed approval occurrence，执行已批准调用并继续运行。
    pub fn resume_pending_approval(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
    ) -> AgentLoopResult {
        self.resume_pending_approval_internal(input, pending, None)
    }

    /// 恢复 approval，并在真实边界向调用方投影有序 typed 事件。
    pub fn resume_pending_approval_with_events(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        on_event: &mut AgentLoopEventCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_pending_approval_internal(input, pending, Some(on_event))
    }

    /// 恢复 approval，并只向调用方投影恢复后最终化 assistant 回合的有序文本 delta。
    pub fn resume_pending_approval_with_text_deltas(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        on_text_delta: &mut AgentTextDeltaCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_pending_approval_internal(
            input,
            pending,
            Some(&mut |event| {
                if let AgentLoopEvent::FinalTextDelta { delta } = event {
                    on_text_delta(&delta);
                }
                Ok(())
            }),
        )
    }

    fn resume_pending_approval_internal(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        mut on_event: Option<&mut AgentLoopEventCallback<'_>>,
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
            .pending_tool_call()
            .to_model_tool_call()
            .map_err(|error| format!("invalid pending tool call arguments: {error}"))
            .and_then(|call| {
                self.tool_broker
                    .validate_execution_input(&call.tool_name, &call.arguments)
                    .map_err(|error| format!("invalid pending execution input: {}", error.code))?;
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
        let (state, model_turn_offset) = match restore_checkpoint(input, pending, &self.tool_broker)
        {
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
        if let Some(workspace_tools) = &self.workspace_tools
            && let Err(error) = workspace_tools
                .bind_checkpoint_workspace_revision(state.completion.workspace_revision)
        {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(format!(
                    "approval checkpoint workspace revision binding failed: {error}"
                )),
            );
        }
        let (capabilities, mut state) = match self.negotiate_tool_capabilities(
            input,
            state,
            model_turn_offset,
            &mut on_event,
        ) {
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
        let tool_call_fingerprint = tool_call_fingerprint(&call);
        let prepared = self.prepare_tool_call(&call, &tool_call_fingerprint, false, &mut state);
        if prepared.rejection.is_some() || prepared.bound.is_none() {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some("pending tool call no longer satisfies its execution boundary".to_string()),
            );
        }
        let occurrence = ModelToolOccurrence {
            call: call.clone(),
            fingerprint: tool_call_fingerprint,
            invalid_was_observed: false,
            context: tool_occurrence_context(input, &call, model_turn_offset.saturating_sub(1), 0),
        };
        if emit_event(
            &mut on_event,
            tool_call_event(&occurrence.context, occurrence.context.timer.started()),
        )
        .is_err()
        {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(EVENT_SINK_FAILURE_ERROR.to_string()),
            );
        }
        let policy_timer = OccurrenceTimer::start();
        let policy_identity =
            child_occurrence_identity(&occurrence.context.identity, "policy_decision", 1);
        let bound = prepared.bound.as_ref().expect("pending call remains bound");
        let operation_count = tool_operation_count(bound, &self.policy.profile);
        let resource_count = u32::try_from(bound.resources.len()).unwrap_or(u32::MAX);
        if emit_event(
            &mut on_event,
            AgentLoopEvent::Observation(AgentObservation::PolicyDecision(
                PolicyDecisionObservation {
                    identity: policy_identity.clone(),
                    lifecycle: policy_timer.started(),
                    operation_count,
                    resource_count,
                    cause: None,
                },
            )),
        )
        .is_err()
        {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(EVENT_SINK_FAILURE_ERROR.to_string()),
            );
        }
        let observed_decision =
            self.tool_decision(input, &prepared, &mut state.used_approval_grants);
        if emit_event(
            &mut on_event,
            AgentLoopEvent::Observation(AgentObservation::PolicyDecision(
                PolicyDecisionObservation {
                    identity: policy_identity,
                    lifecycle: policy_timer.finished(policy_status(&observed_decision.decision)),
                    operation_count,
                    resource_count,
                    cause: Some(observed_decision.cause),
                },
            )),
        )
        .is_err()
        {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some(EVENT_SINK_FAILURE_ERROR.to_string()),
            );
        }
        if !matches!(
            observed_decision.decision,
            ToolBrokerDecision::Approved { .. }
        ) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some("pending tool call approval did not match".to_string()),
            );
        }
        let runtime = self.execute_tool(
            &prepared,
            observed_decision.decision,
            &mut state,
            &occurrence.context,
            &mut on_event,
        );
        match self.record_tool_results(
            input,
            &mut state,
            vec![(prepared, runtime)],
            &[occurrence],
            false,
            &mut on_event,
        ) {
            ToolBatchControl::Continue => {}
            ToolBatchControl::Cancelled => {
                return state.finish(AgentStatus::Cancelled, false, None, model_turn_offset, None);
            }
            ToolBatchControl::Failed(error) => {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    model_turn_offset,
                    Some(error),
                );
            }
            ToolBatchControl::Blocked => unreachable!("resumed tool cannot block in execution"),
        }
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            model_turn_offset,
            on_event,
        )
    }

    /// 协商模型提供方能力并记录结果，然后构建模型上下文。
    fn negotiate_tool_capabilities(
        &self,
        input: &AgentLoopInput,
        mut state: AgentLoopState,
        model_turns: u32,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
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
        let provider_events =
            RefCell::new(ProviderEventBridge::new_root(input, model_turns, on_event));
        let negotiation = {
            let mut on_attempt = |event| provider_events.borrow_mut().on_attempt(event);
            self.provider.negotiate_tool_capabilities_observed(
                &input.model_preferences,
                &self.cancellation,
                &mut on_attempt,
            )
        };
        let provider_events = provider_events.into_inner();
        if provider_events.event_sink_failed || provider_events.active_attempt.is_some() {
            return ControlFlow::Break(state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turns,
                Some(EVENT_SINK_FAILURE_ERROR.to_string()),
            ));
        }
        match negotiation {
            Ok(negotiation) => {
                state.record_provider_negotiation(
                    model_turns,
                    &negotiation.contract,
                    &negotiation.metadata,
                );
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
                state.record_provider_negotiation_error(model_turns, &error);
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
        prepared: &PreparedToolCall,
        used_approval_grants: &mut BTreeSet<String>,
    ) -> ObservedToolDecision {
        let call = &prepared.call;
        if call.parse_status != ModelToolParseStatus::Valid || !call.arguments.is_object() {
            return ObservedToolDecision {
                decision: ToolBrokerDecision::deny_with_kind(
                    ToolFailureKind::Input,
                    "invalid tool call arguments",
                ),
                cause: PolicyDecisionCause::Explicit,
            };
        }
        let Some(bound) = &prepared.bound else {
            return ObservedToolDecision {
                decision: ToolBrokerDecision::deny_with_kind(
                    ToolFailureKind::Input,
                    "tool call was not bound by the registry",
                ),
                cause: PolicyDecisionCause::Explicit,
            };
        };
        let request_id = approval_request_id(input, call);
        let permission = self.tool_permission_decision(bound);
        if used_approval_grants.contains(&request_id) {
            return ObservedToolDecision {
                decision: ToolBrokerDecision::deny_with_kind(
                    ToolFailureKind::Approval,
                    "approval grant already consumed",
                ),
                cause: PolicyDecisionCause::ApprovalState,
            };
        }
        if !matches!(permission.outcome, PermissionDecisionOutcome::Deny)
            && let Some(grant) = input.approval_grants.iter().find(|grant| {
                grant.request_id == request_id
                    && grant.tool_name == bound.tool_id
                    && grant.resources == bound.resources
                    && matches!(grant.outcome, ApprovalOutcome::Allow)
            })
        {
            used_approval_grants.insert(grant.request_id.clone());
            return ObservedToolDecision {
                decision: ToolBrokerDecision::approved(grant.request_id.clone()),
                cause: PolicyDecisionCause::ApprovalGrant,
            };
        }
        let cause = safe_policy_cause(&permission.cause);
        let decision = match permission.outcome {
            PermissionDecisionOutcome::Allow => ToolBrokerDecision::Allow,
            PermissionDecisionOutcome::Deny => ToolBrokerDecision::deny_with_kind(
                permission_failure_kind(&permission.cause),
                permission.reason,
            ),
            PermissionDecisionOutcome::Ask => {
                ToolBrokerDecision::ask(request_id, permission.reason)
            }
        };
        ObservedToolDecision { decision, cause }
    }

    fn tool_permission_decision(&self, bound: &BoundToolCall) -> PermissionDecision {
        let mut operations = vec![bound.operation];
        if matches!(
            bound.executor,
            ToolExecutor::Workspace(WorkspaceToolExecutor::Command)
        ) && self.policy.profile.network_access == NetworkAccess::Allowed
        {
            operations.push(PermissionOperation::Network);
        }
        let mut first_allow = None;
        let mut first_ask = None;
        for operation in operations {
            for resource in &bound.resources {
                let mut request =
                    PermissionRequest::new(bound.tool_id.clone(), operation, resource.clone());
                if bound.resource_is_sensitive(resource) {
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
        occurrences: &[ModelToolOccurrence],
        state: &mut AgentLoopState,
        next_model_turn: u32,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    ) -> ToolBatchControl {
        if self.is_cancelled(input) {
            return ToolBatchControl::Cancelled;
        }
        let mut prepared = occurrences
            .iter()
            .map(|occurrence| {
                self.prepare_tool_call(
                    &occurrence.call,
                    &occurrence.fingerprint,
                    occurrence.invalid_was_observed,
                    state,
                )
            })
            .collect::<Vec<_>>();
        if self.is_cancelled(input) {
            return ToolBatchControl::Cancelled;
        }

        let mut staged_approval_grants = state.used_approval_grants.clone();
        if !prepared.iter().any(|call| call.rejection.is_some()) {
            for (prepared_call, occurrence) in prepared.iter_mut().zip(occurrences) {
                let policy_timer = OccurrenceTimer::start();
                let identity =
                    child_occurrence_identity(&occurrence.context.identity, "policy_decision", 0);
                let operation_count = tool_operation_count(
                    prepared_call
                        .bound
                        .as_ref()
                        .expect("prepared call is bound"),
                    &self.policy.profile,
                );
                let resource_count = prepared_call.bound.as_ref().map_or(0, |bound| {
                    u32::try_from(bound.resources.len()).unwrap_or(u32::MAX)
                });
                if emit_event(
                    on_event,
                    AgentLoopEvent::Observation(AgentObservation::PolicyDecision(
                        PolicyDecisionObservation {
                            identity: identity.clone(),
                            lifecycle: policy_timer.started(),
                            operation_count,
                            resource_count,
                            cause: None,
                        },
                    )),
                )
                .is_err()
                {
                    return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
                }
                let observed_decision =
                    self.tool_decision(input, prepared_call, &mut staged_approval_grants);
                if emit_event(
                    on_event,
                    AgentLoopEvent::Observation(AgentObservation::PolicyDecision(
                        PolicyDecisionObservation {
                            identity,
                            lifecycle: policy_timer
                                .finished(policy_status(&observed_decision.decision)),
                            operation_count,
                            resource_count,
                            cause: Some(observed_decision.cause),
                        },
                    )),
                )
                .is_err()
                {
                    return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
                }
                let decision = observed_decision.decision;
                prepared_call.rejection = matches!(decision, ToolBrokerDecision::Deny { .. })
                    .then(|| self.decision_result(&prepared_call.call, &decision));
                prepared_call.decision = Some(decision);
            }
        }

        if prepared.len() > 1
            && prepared.iter().any(|call| {
                call.rejection.is_some()
                    || call.bound.as_ref().map(|bound| bound.execution_mode)
                        == Some(ToolExecutionMode::Exclusive)
                    || matches!(call.decision, Some(ToolBrokerDecision::Ask { .. }))
            })
        {
            let results = prepared
                .drain(..)
                .map(|call| {
                    let result = self.batch_rejection_result(&call);
                    (
                        call,
                        RuntimeToolResult {
                            result,
                            duration_ms: None,
                            event_sink_failed: false,
                        },
                    )
                })
                .collect::<Vec<_>>();
            return self.record_tool_results(input, state, results, occurrences, true, on_event);
        }

        if prepared.len() > 1 {
            state.used_approval_grants = staged_approval_grants;
            let results = self.execute_parallel_reads(prepared);
            if self.is_cancelled(input) {
                return ToolBatchControl::Cancelled;
            }
            return self.record_tool_results(input, state, results, occurrences, false, on_event);
        }

        let Some(prepared) = prepared.pop() else {
            return ToolBatchControl::Continue;
        };
        if let Some(result) = prepared.rejection.clone() {
            return self.record_tool_results(
                input,
                state,
                vec![(
                    prepared,
                    RuntimeToolResult {
                        result,
                        duration_ms: None,
                        event_sink_failed: false,
                    },
                )],
                occurrences,
                false,
                on_event,
            );
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
            let request = approval_request(input, approval_request_id, &prepared, reason);
            let pending = PendingToolCall::new(
                input,
                &prepared.call,
                prepared
                    .bound
                    .as_ref()
                    .expect("admitted tool call is bound"),
            );
            let checkpoint = match state.checkpoint(input, &pending, next_model_turn) {
                Ok(checkpoint) => checkpoint,
                Err(error) => return ToolBatchControl::Failed(error),
            };
            let occurrence = match PendingApprovalOccurrence::new(request, checkpoint) {
                Ok(occurrence) => occurrence,
                Err(error) => return ToolBatchControl::Failed(error),
            };
            state.pending_approvals.push(occurrence);
            let result = self.execute_tool(
                &prepared,
                decision,
                state,
                &occurrences
                    .first()
                    .expect("single approval occurrence is present")
                    .context,
                on_event,
            );
            state.append_hidden_tool_result(result.result);
            if emit_event(
                on_event,
                tool_call_event(
                    &occurrences
                        .first()
                        .expect("single approval occurrence is present")
                        .context,
                    occurrences
                        .first()
                        .expect("single approval occurrence is present")
                        .context
                        .timer
                        .suspended(ToolCallStatus::ApprovalRequired),
                ),
            )
            .is_err()
            {
                return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
            }
            return ToolBatchControl::Blocked;
        }
        state.used_approval_grants = staged_approval_grants;
        let result = self.execute_tool(
            &prepared,
            decision,
            state,
            &occurrences
                .first()
                .expect("single tool occurrence is present")
                .context,
            on_event,
        );
        self.record_tool_results(
            input,
            state,
            vec![(prepared, result)],
            occurrences,
            false,
            on_event,
        )
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
                bound: None,
                decision: None,
                rejection: Some(invalid_tool_arguments_result(
                    execution_call,
                    ToolInputValidationError::new(validation_code),
                    self.tool_broker.get(&execution_call.tool_name),
                )),
            };
        }
        let (_, execution_arguments) = match self
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
                    bound: None,
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
        let (filesystem, network) = effective_command_policy(&self.policy.profile);
        let bound = match self.tool_broker.bind_authorization(
            &bound_call.tool_name,
            bound_call.arguments.clone(),
            self.workspace_tools.as_ref(),
            filesystem,
            network,
        ) {
            Ok(bound) => bound,
            Err(error) => {
                let rejection = self.workspace_binding_rejection(&bound_call, error);
                return PreparedToolCall {
                    call: bound_call,
                    fingerprint: fingerprint.to_string(),
                    bound: None,
                    decision: None,
                    rejection: Some(rejection),
                };
            }
        };
        bound_call.arguments = bound.arguments.clone();
        bound_call.raw_arguments = bound.arguments.to_string();
        if let Err(error) = self
            .tool_broker
            .validate_execution_input(&bound_call.tool_name, &bound_call.arguments)
        {
            if !invalid_was_observed {
                state.recovery_metrics.invalid_tool_call_count = state
                    .recovery_metrics
                    .invalid_tool_call_count
                    .saturating_add(1);
            }
            return PreparedToolCall {
                call: bound_call,
                fingerprint: fingerprint.to_string(),
                bound: None,
                decision: None,
                rejection: Some(invalid_tool_arguments_result(
                    execution_call,
                    error,
                    self.tool_broker.get(&execution_call.tool_name),
                )),
            };
        }
        PreparedToolCall {
            call: bound_call,
            fingerprint: fingerprint.to_string(),
            bound: Some(bound),
            decision: None,
            rejection: None,
        }
    }

    fn workspace_binding_rejection(
        &self,
        call: &ModelToolCall,
        error: WorkspaceToolError,
    ) -> ToolResult {
        let envelope = tool_call_request(call);
        let output = if call.tool_name == TOOL_COMMAND {
            match command_tool_input(&call.arguments) {
                Ok(input) => {
                    command_workspace_tool_failure(&input, error.into(), &self.policy.profile)
                }
                Err(_) => workspace_tool_failure(error.into()),
            }
        } else {
            workspace_tool_failure(error.into())
        };
        ToolResult::from_result(&envelope, &output)
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
            _ if prepared.bound.as_ref().map(|bound| bound.execution_mode)
                == Some(ToolExecutionMode::Exclusive) =>
            {
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
                .execute(&tool_call_request(call), decision.clone(), |_, _| {
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
    ) -> Vec<(PreparedToolCall, RuntimeToolResult)> {
        let broker = &self.tool_broker;
        let workspace_tools = self.workspace_tools.as_ref();
        let cancellation = &self.cancellation;
        let results = parallel_map(prepared.clone(), |worker| {
            let started = std::time::Instant::now();
            let decision = worker
                .decision
                .clone()
                .expect("admitted parallel read has a policy decision");
            let envelope = tool_call_request(&worker.call);
            let bound = worker
                .bound
                .as_ref()
                .expect("admitted parallel read is registry-bound");
            let result = broker.execute(&envelope, decision.clone(), |executor, _| {
                execute_workspace_tool_call(
                    workspace_tools,
                    cancellation,
                    &worker.call,
                    executor,
                    WorkspaceToolCallContext {
                        bound,
                        decision: &decision,
                        profile: &PermissionProfile::workspace_write(),
                        occurrence: None,
                    },
                    None,
                )
                .output
            });
            RuntimeToolResult {
                result,
                duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
                event_sink_failed: false,
            }
        });
        prepared
            .into_iter()
            .zip(results)
            .map(|(backup, result)| {
                let result = result.unwrap_or_else(|| RuntimeToolResult {
                    result: ToolResult::failed_with_kind(
                        &tool_call_request(&backup.call),
                        ToolFailureKind::Infrastructure,
                        "parallel_read_worker_failed",
                        "parallel read worker failed",
                    ),
                    duration_ms: None,
                    event_sink_failed: false,
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
        results: Vec<(PreparedToolCall, RuntimeToolResult)>,
        occurrences: &[ModelToolOccurrence],
        batch_rejected: bool,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    ) -> ToolBatchControl {
        debug_assert_eq!(results.len(), occurrences.len());
        let mut failure = None;
        let mut repairable_failure = None;
        for ((prepared, runtime), occurrence) in results.into_iter().zip(occurrences) {
            if runtime.event_sink_failed {
                return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
            }
            let tool_duration_ms = runtime
                .duration_ms
                .unwrap_or_else(|| occurrence.context.timer.elapsed_ms());
            let result = runtime.result;
            let result = if self.is_cancelled(input) && result.ok {
                cancelled_tool_result(&prepared.call)
            } else {
                result
            };
            let recoverable = is_repairable_tool_result(&result)
                || (batch_rejected && result.failure_kind == Some(ToolFailureKind::Approval));
            let non_repairable_error = (!result.ok && !recoverable).then(|| {
                result
                    .error_code
                    .clone()
                    .unwrap_or_else(|| "tool_execution_failed".to_string())
            });
            let verification_identity = (prepared.call.tool_name == TOOL_COMMAND).then(|| {
                child_occurrence_identity(&occurrence.context.identity, "verification", 0)
            });
            // The occurrence ordinal identifies this verification span; the successful-command
            // total is a mutable metric and cannot be used as a Start/End identity attribute.
            let verification_occurrence_count =
                occurrence.context.tool_call_ordinal.saturating_add(1);
            if let Some(identity) = &verification_identity {
                let summary = state.completion.summary();
                if emit_event(
                    on_event,
                    AgentLoopEvent::Observation(AgentObservation::Verification(
                        VerificationObservation {
                            identity: identity.clone(),
                            lifecycle: occurrence.context.timer.started(),
                            required_command_count: summary.required_command_count,
                            satisfied_command_count: summary.satisfied_command_count,
                            occurrence_count: verification_occurrence_count,
                            command_duration_ms: Some(tool_duration_ms),
                        },
                    )),
                )
                .is_err()
                {
                    return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
                }
            }
            let recovery_feedback = state.observe_tool_result(&result, &prepared.fingerprint);
            if let Some(identity) = verification_identity {
                let summary = state.completion.summary();
                let status = if result.ok {
                    VerificationStatus::CommandPassed
                } else {
                    VerificationStatus::CommandFailed
                };
                if emit_event(
                    on_event,
                    AgentLoopEvent::Observation(AgentObservation::Verification(
                        VerificationObservation {
                            identity,
                            lifecycle: occurrence
                                .context
                                .timer
                                .finished_with_duration(tool_duration_ms, status),
                            required_command_count: summary.required_command_count,
                            satisfied_command_count: summary.satisfied_command_count,
                            occurrence_count: verification_occurrence_count,
                            command_duration_ms: Some(tool_duration_ms),
                        },
                    )),
                )
                .is_err()
                {
                    return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
                }
            }
            if !result.ok && is_repairable_tool_result(&result) {
                repairable_failure = state.last_repair_failure.clone();
            }
            let provider_tool_name = (prepared.call.parse_status != ModelToolParseStatus::Valid)
                .then_some(PROVIDER_HISTORY_REJECTED_TOOL);
            state.append_visible_tool_result(result, provider_tool_name);
            if let Some(feedback) = recovery_feedback {
                state
                    .messages
                    .push(ModelMessage::text(ModelRole::Developer, feedback));
            }
            if failure.is_none() {
                failure = non_repairable_error;
            }
            let status = tool_result_status(
                &prepared,
                state
                    .tool_result_occurrences
                    .last()
                    .expect("recorded tool occurrence")
                    .result(),
                batch_rejected,
            );
            if emit_event(
                on_event,
                tool_call_event(
                    &occurrence.context,
                    occurrence
                        .context
                        .timer
                        .finished_with_duration(tool_duration_ms, status),
                ),
            )
            .is_err()
            {
                return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
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
        prepared: &PreparedToolCall,
        decision: ToolBrokerDecision,
        state: &mut AgentLoopState,
        occurrence: &ToolOccurrenceContext,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    ) -> RuntimeToolResult {
        let started = std::time::Instant::now();
        let call = &prepared.call;
        let bound = prepared
            .bound
            .as_ref()
            .expect("admitted tool call is registry-bound");
        let envelope = tool_call_request(call);
        let executor_decision = decision.clone();
        let mut sandbox_execution = None;
        let mut event_sink_failed = false;
        let mut result = self
            .tool_broker
            .execute(&envelope, decision.clone(), |executor, _| match executor {
                ToolExecutor::AgentControl(AgentControlToolExecutor::UpdatePlan) => {
                    self.execute_plan_update(call, state)
                }
                ToolExecutor::Workspace(_) => {
                    let execution = self.execute_workspace_tool(
                        call,
                        executor,
                        bound,
                        &executor_decision,
                        occurrence,
                        on_event,
                    );
                    sandbox_execution = execution.sandbox_execution;
                    event_sink_failed = execution.event_sink_failed;
                    execution.output
                }
            });
        if matches!(
            bound.executor,
            ToolExecutor::Workspace(WorkspaceToolExecutor::Command)
        ) {
            let existing_audit = result.audit_metadata().cloned();
            result = result.with_audit(command_audit_metadata(
                existing_audit.as_ref(),
                call,
                &decision,
                self.policy.profile.approval_policy,
                &self.policy.profile,
            ));
        }
        let duration_ms = sandbox_execution.as_ref().map_or_else(
            || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            |sandbox| sandbox.duration_ms,
        );
        RuntimeToolResult {
            result,
            duration_ms: Some(duration_ms),
            event_sink_failed,
        }
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
        executor: ToolExecutor,
        bound: &BoundToolCall,
        decision: &ToolBrokerDecision,
        occurrence: &ToolOccurrenceContext,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    ) -> WorkspaceToolExecution {
        execute_workspace_tool_call(
            self.workspace_tools.as_ref(),
            &self.cancellation,
            call,
            executor,
            WorkspaceToolCallContext {
                bound,
                decision,
                profile: &self.policy.profile,
                occurrence: Some(occurrence),
            },
            Some(on_event),
        )
    }
}

fn execute_workspace_tool_call(
    workspace_tools: Option<&WorkspaceTools>,
    cancellation: &CancellationToken,
    call: &ModelToolCall,
    executor: ToolExecutor,
    context: WorkspaceToolCallContext<'_>,
    on_event: Option<&mut Option<&mut AgentLoopEventCallback<'_>>>,
) -> WorkspaceToolExecution {
    let WorkspaceToolCallContext {
        bound,
        decision,
        profile,
        occurrence,
    } = context;
    if cancellation.is_cancelled() {
        return WorkspaceToolExecution {
            output: ToolOutput::failure_with_kind(
                ToolFailureKind::Cancelled,
                "tool_cancelled",
                json!({"summary": "tool execution cancelled"}),
            ),
            sandbox_execution: None,
            event_sink_failed: false,
        };
    }
    let Some(workspace_tools) = workspace_tools else {
        return WorkspaceToolExecution {
            output: ToolOutput::failure_with_kind(
                ToolFailureKind::Backend,
                "backend_unavailable",
                json!({"summary": "workspace tool backend is unavailable"}),
            ),
            sandbox_execution: None,
            event_sink_failed: false,
        };
    };
    let mut sandbox_execution = None;
    let mut event_sink_failed = false;
    let result = match executor {
        ToolExecutor::Workspace(WorkspaceToolExecutor::Read) => read_tool_input(&call.arguments)
            .and_then(|input| {
                workspace_tools
                    .read_cancellable(input, cancellation)
                    .map_err(Into::into)
            }),
        ToolExecutor::Workspace(WorkspaceToolExecutor::List) => list_tool_input(&call.arguments)
            .and_then(|input| {
                workspace_tools
                    .list_cancellable(input, cancellation)
                    .map_err(Into::into)
            }),
        ToolExecutor::Workspace(WorkspaceToolExecutor::Grep) => grep_tool_input(&call.arguments)
            .and_then(|input| {
                workspace_tools
                    .grep_cancellable(input, cancellation)
                    .map_err(Into::into)
            }),
        ToolExecutor::Workspace(WorkspaceToolExecutor::Edit) => edit_tool_input(&call.arguments)
            .and_then(|input| workspace_tools.edit(input, decision).map_err(Into::into)),
        ToolExecutor::Workspace(WorkspaceToolExecutor::Patch) => patch_tool_input(&call.arguments)
            .and_then(|input| workspace_tools.patch(input, decision).map_err(Into::into)),
        ToolExecutor::Workspace(WorkspaceToolExecutor::Command) => {
            match command_tool_input(&call.arguments) {
                Ok(input) => {
                    let (filesystem, network) = effective_command_policy(profile);
                    let Some(expected_scope) =
                        bound.resources.iter().find_map(|resource| match resource {
                            PermissionResource::CommandScope(digest) => Some(digest),
                            PermissionResource::WorkspacePath(_) | PermissionResource::Tool(_) => {
                                None
                            }
                        })
                    else {
                        return WorkspaceToolExecution {
                            output: ToolOutput::failure_with_kind(
                                ToolFailureKind::PermissionProfile,
                                "command_scope_missing",
                                json!({"summary": "command authorization scope is missing"}),
                            ),
                            sandbox_execution: None,
                            event_sink_failed: false,
                        };
                    };
                    let execution = if let (Some(occurrence), Some(agent_sink)) =
                        (occurrence, on_event)
                    {
                        let mut sandbox_sink = |boundary| {
                            if emit_event(agent_sink, sandbox_boundary_event(occurrence, boundary))
                                .is_err()
                            {
                                event_sink_failed = true;
                                Err(ToolSandboxExecutionSinkError)
                            } else {
                                Ok(())
                            }
                        };
                        workspace_tools.command_cancellable_with_policy_events(
                            input.clone(),
                            filesystem,
                            network,
                            expected_scope,
                            cancellation,
                            &mut sandbox_sink,
                        )
                    } else {
                        workspace_tools.command_cancellable_with_policy_observed(
                            input.clone(),
                            filesystem,
                            network,
                            expected_scope,
                            cancellation,
                        )
                    };
                    Ok(match execution {
                        Ok(execution) => {
                            sandbox_execution = Some(execution.sandbox_execution);
                            execution.output
                        }
                        Err(WorkspaceToolError::ObservationSinkFailed) if event_sink_failed => {
                            ToolOutput::failure_with_kind(
                                ToolFailureKind::Infrastructure,
                                "event_sink_failed",
                                json!({"summary": EVENT_SINK_FAILURE_ERROR}),
                            )
                        }
                        Err(error) => command_workspace_tool_failure(&input, error.into(), profile),
                    })
                }
                Err(error) => Err(error),
            }
        }
        ToolExecutor::AgentControl(_) => Ok(ToolOutput::failure_with_kind(
            ToolFailureKind::Backend,
            "backend_unavailable",
            json!({"summary": "tool backend is unavailable"}),
        )),
    };
    WorkspaceToolExecution {
        output: result.unwrap_or_else(workspace_tool_failure),
        sandbox_execution,
        event_sink_failed,
    }
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
    tool_result_occurrences: &[ToolResultOccurrence],
    budget: &ContextBudget,
) -> bool {
    model_request_token_count(tools, messages, tool_result_occurrences, budget)
        <= budget.model_context_window
}

fn model_request_token_count(
    tools: &[ModelToolSchema],
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
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
    let tool_result_accounting =
        tool_result_context_token_adjustment(messages, tool_result_occurrences);
    let message_framing = u32::try_from(messages.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    payload_tokens
        .saturating_add(tool_result_accounting)
        .saturating_add(budget.reserved_output_tokens)
        .saturating_add(message_framing)
        .saturating_add(budget.fixed_overhead_tokens)
}

/// 将真实追加顺序中的 tool occurrence 与安全结果 accounting 对齐；压缩占位消息不重复计入。
fn tool_result_context_token_adjustment(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
) -> u32 {
    let Some(occurrences) = tool_result_message_occurrences(messages, tool_result_occurrences)
    else {
        return u32::MAX;
    };
    occurrences
        .into_iter()
        .filter_map(|occurrence| {
            let message_index = occurrence.tool_index?;
            let occurrence_record = tool_result_occurrences.get(occurrence.result_index)?;
            if occurrence_record.visibility() != ToolResultVisibility::Visible {
                return None;
            }
            let message = messages.get(message_index)?;
            let context_token_count = occurrence_record.result().context_token_count()?;
            Some(context_token_count.saturating_sub(approximate_token_count(&message.content)))
        })
        .fold(0, u32::saturating_add)
}

#[derive(Debug, Clone, Copy)]
struct ToolResultMessageOccurrence {
    assistant_index: usize,
    tool_index: Option<usize>,
    result_index: usize,
    visibility: ToolResultVisibility,
}

/// 按 occurrence 顺序验证当前 tool message 与结果的一一绑定。
fn tool_result_message_occurrences(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
) -> Option<Vec<ToolResultMessageOccurrence>> {
    if tool_result_occurrences
        .iter()
        .any(|occurrence| occurrence.validate().is_err())
    {
        return None;
    }
    if messages.iter().any(|message| {
        message.role == ModelRole::Assistant && has_duplicate_tool_call_ids(&message.tool_calls)
    }) {
        return None;
    }
    let assistant_calls = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == ModelRole::Assistant)
        .flat_map(|(assistant_index, message)| {
            message
                .tool_calls
                .iter()
                .enumerate()
                .map(move |(call_index, call)| {
                    (assistant_index, call_index, call.tool_call_id.as_str())
                })
        })
        .collect::<Vec<_>>();
    let result_occurrences = tool_result_occurrences
        .iter()
        .enumerate()
        .filter_map(|(index, occurrence)| {
            (occurrence.visibility() != ToolResultVisibility::Omitted)
                .then_some((index, occurrence.visibility()))
        })
        .collect::<Vec<_>>();
    if assistant_calls.len() != result_occurrences.len() {
        return None;
    }
    let mut occurrences = assistant_calls
        .into_iter()
        .zip(result_occurrences)
        .map(
            |((assistant_index, _call_index, call_id), (result_index, visibility))| {
                let result = tool_result_occurrences.get(result_index)?.result();
                (call_id == result.tool_call_id).then_some(ToolResultMessageOccurrence {
                    assistant_index,
                    tool_index: None,
                    result_index,
                    visibility,
                })
            },
        )
        .collect::<Option<Vec<_>>>()?;

    let tool_message_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == ModelRole::Tool).then_some(index))
        .collect::<Vec<_>>();
    let visible_result_occurrences = occurrences
        .iter()
        .filter(|occurrence| {
            matches!(
                occurrence.visibility,
                ToolResultVisibility::Visible | ToolResultVisibility::Compacted
            )
        })
        .map(|occurrence| occurrence.result_index)
        .collect::<Vec<_>>();
    if tool_message_indices.len() != visible_result_occurrences.len() {
        return None;
    }
    for (tool_index, result_index) in tool_message_indices
        .into_iter()
        .zip(visible_result_occurrences)
    {
        let message = messages.get(tool_index)?;
        let result = tool_result_occurrences.get(result_index)?.result();
        if message.tool_call_id.as_deref() != Some(result.tool_call_id.as_str()) {
            return None;
        }
        let occurrence = occurrences
            .iter_mut()
            .find(|occurrence| occurrence.result_index == result_index)?;
        if occurrence.tool_index.replace(tool_index).is_some() {
            return None;
        }
        match occurrence.visibility {
            ToolResultVisibility::Visible if has_compaction_marker(message) => {
                return None;
            }
            ToolResultVisibility::Compacted if !is_compacted_tool_result_message(message) => {
                return None;
            }
            ToolResultVisibility::Hidden | ToolResultVisibility::Omitted => {
                return None;
            }
            ToolResultVisibility::Visible | ToolResultVisibility::Compacted => {}
        }
    }
    if occurrences.iter().any(|occurrence| {
        matches!(
            occurrence.visibility,
            ToolResultVisibility::Visible | ToolResultVisibility::Compacted
        ) && occurrence.tool_index.is_none()
    }) {
        return None;
    }
    Some(occurrences)
}

#[derive(Debug)]
struct ContextCompactionOutcome {
    messages: Vec<ModelMessage>,
    tool_result_occurrences: Vec<ToolResultOccurrence>,
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
    let before_tokens = model_request_token_count(
        tools,
        &state.messages,
        &state.tool_result_occurrences,
        budget,
    );
    if before_tokens <= budget.model_context_window {
        return None;
    }
    let occurrences =
        tool_result_message_occurrences(&state.messages, &state.tool_result_occurrences)?;

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
    let latest_tool_assistant = latest_complete_tool_assistant(&occurrences);

    let mut preserved_indices = authority_indices.clone();
    preserved_indices.insert(current_user_index);
    if let Some(assistant_index) = latest_tool_assistant {
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
    let mut tool_result_occurrences = state
        .tool_result_occurrences
        .iter()
        .cloned()
        .map(|mut occurrence| {
            occurrence.set_visibility(ToolResultVisibility::Omitted);
            occurrence
        })
        .collect::<Vec<_>>();
    if let Some(assistant_index) = latest_tool_assistant
        && assistant_index > current_user_index
    {
        messages.push(state.messages[assistant_index].clone());
        for occurrence in occurrences.iter().filter(|occurrence| {
            occurrence.assistant_index == assistant_index
                && matches!(
                    occurrence.visibility,
                    ToolResultVisibility::Visible | ToolResultVisibility::Compacted
                )
        }) {
            let tool_index = occurrence.tool_index?;
            let result_index = occurrence.result_index;
            messages.push(compacted_tool_result_message(
                &state.messages[tool_index],
                state.tool_result_occurrences.get(result_index),
            ));
            let occurrence = tool_result_occurrences.get_mut(result_index)?;
            occurrence.set_visibility(ToolResultVisibility::Compacted);
        }
    }

    let after_tokens =
        model_request_token_count(tools, &messages, &tool_result_occurrences, budget);
    if after_tokens >= before_tokens || after_tokens > budget.model_context_window {
        return None;
    }
    Some(ContextCompactionOutcome {
        messages,
        tool_result_occurrences,
        compacted_message_count,
        before_tokens,
        after_tokens,
    })
}

fn latest_complete_tool_assistant(occurrences: &[ToolResultMessageOccurrence]) -> Option<usize> {
    for occurrence in occurrences.iter().rev() {
        if matches!(
            occurrence.visibility,
            ToolResultVisibility::Visible | ToolResultVisibility::Compacted
        ) {
            occurrence.tool_index?;
            return Some(occurrence.assistant_index);
        }
    }
    None
}

/// 识别 tool 消息是否声称为 compaction 占位。
fn has_compaction_marker(message: &ModelMessage) -> bool {
    message.role == ModelRole::Tool
        && serde_json::from_str::<Value>(&message.content)
            .ok()
            .and_then(|payload| payload.get("compacted").and_then(Value::as_bool))
            == Some(true)
}

/// 只接受 AgentLoop 自己生成的有界 compaction tool 占位。
fn is_compacted_tool_result_message(message: &ModelMessage) -> bool {
    if message.role != ModelRole::Tool {
        return false;
    }
    let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message.content) else {
        return false;
    };
    payload.len() == 5
        && payload.get("compacted") == Some(&Value::Bool(true))
        && payload.get("ok").is_some_and(Value::is_boolean)
        && payload
            .get("error_code")
            .is_some_and(|value| value.is_null() || value.is_string())
        && payload.get("truncated").is_some_and(Value::is_boolean)
        && payload.get("instruction").and_then(Value::as_str)
            == Some(COMPACTED_TOOL_RESULT_INSTRUCTION)
}

fn compacted_tool_result_message(
    original: &ModelMessage,
    occurrence: Option<&ToolResultOccurrence>,
) -> ModelMessage {
    let content = json!({
        "compacted": true,
        "ok": occurrence.is_some_and(|occurrence| occurrence.result().ok),
        "error_code": occurrence.and_then(|occurrence| occurrence.result().error_code.as_deref()),
        "truncated": occurrence.is_some_and(|occurrence| occurrence.result().truncated),
        "instruction": COMPACTED_TOOL_RESULT_INSTRUCTION
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
        .tool_result_occurrences
        .iter()
        .filter(|occurrence| !occurrence.result().ok)
        .count();
    json!({
        "type": "agent_context_compaction",
        "notice": "Older messages and raw tool output were omitted. Do not assume omitted evidence; inspect the workspace again when needed.",
        "compacted_message_count": compacted_message_count,
        "tool_result_count": state.tool_result_occurrences.len(),
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

/// 在已批准调用执行前恢复并重新规范化检查点。
fn restore_checkpoint(
    input: &AgentLoopInput,
    pending: &PendingApprovalOccurrence,
    tool_broker: &ToolBroker,
) -> Result<(AgentLoopState, u32), String> {
    pending.validate_binding()?;
    let checkpoint = pending.checkpoint().clone();
    let pending_tool_call = pending.pending_tool_call();
    if checkpoint.thread_id != input.thread_id {
        return Err("approval checkpoint thread mismatch".to_string());
    }
    if checkpoint.turn_id != input.turn_id {
        return Err("approval checkpoint turn mismatch".to_string());
    }
    if checkpoint.project_instructions_digest != input.project_instructions_digest {
        return Err("approval checkpoint project instructions digest mismatch".to_string());
    }
    if checkpoint.pending_tool_call != *pending_tool_call {
        return Err("approval checkpoint tool call mismatch".to_string());
    }
    let expected_request_id =
        approval_request_id_from_tool_call_id(&input.turn_id, &pending_tool_call.tool_call_id);
    if pending_tool_call.request_id != expected_request_id
        || checkpoint.pending_tool_call.request_id != expected_request_id
    {
        return Err("approval checkpoint request mismatch".to_string());
    }
    let last_message = checkpoint
        .messages
        .last()
        .ok_or_else(|| "approval checkpoint messages are missing".to_string())?;
    if last_message.role != ModelRole::Assistant
        || last_message.tool_calls.len() != 1
        || last_message.tool_calls[0].tool_call_id != pending_tool_call.tool_call_id
    {
        return Err("approval checkpoint assistant tool-call ordering is invalid".to_string());
    }
    let model_visible_call = &last_message.tool_calls[0];
    if model_visible_call.parse_status != ModelToolParseStatus::Valid {
        return Err("approval checkpoint assistant tool-call name is invalid".to_string());
    }
    let pending_call = pending_tool_call
        .to_model_tool_call()
        .map_err(|error| format!("invalid pending checkpoint tool call arguments: {error}"))?;
    if model_visible_call.tool_name != pending_tool_call.tool_name.as_str() {
        return Err("approval checkpoint assistant tool-call name is invalid".to_string());
    }
    let canonical_model_call = model_visible_call.clone();
    let canonical_model_call = canonicalize_model_tool_call(tool_broker, &canonical_model_call)
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
    if used_approval_grants.contains(&pending_tool_call.request_id) {
        return Err("approval checkpoint consumed the pending grant".to_string());
    }
    let tool_result_occurrences = checkpoint.tool_result_occurrences;
    let checkpoint_history_messages = &checkpoint.messages[..checkpoint.messages.len() - 1];
    if tool_result_message_occurrences(checkpoint_history_messages, &tool_result_occurrences)
        .is_none()
    {
        return Err("approval checkpoint tool result occurrence bindings are invalid".to_string());
    }
    let mut derived_completion =
        CompletionTracker::from_requirements(&input.verification_requirements)?;
    for occurrence in &tool_result_occurrences {
        derived_completion.observe(occurrence.result());
    }
    if !derived_completion.is_consistent() {
        return Err("approval checkpoint derived workspace revision state is invalid".to_string());
    }
    if derived_completion != checkpoint.completion {
        return Err("approval checkpoint completion state mismatch".to_string());
    }
    let derived_plan_update_count = tool_result_occurrences
        .iter()
        .filter(|occurrence| {
            occurrence.result().tool_name == UPDATE_PLAN_TOOL && occurrence.result().ok
        })
        .count() as u32;
    if derived_plan_update_count != checkpoint.plan_update_count {
        return Err("approval checkpoint plan update count mismatch".to_string());
    }
    let seen_tool_call_fingerprints = checkpoint
        .seen_tool_call_fingerprints
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut state = AgentLoopState::new(checkpoint.messages, input.max_turns.max(1), None);
    state.tool_result_occurrences = tool_result_occurrences;
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

fn provider_stream_text_mismatch_error() -> ModelError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider streamed text did not match the completed response",
    )
    .with_provider_diagnostic(
        "provider_stream_text_mismatch",
        ProviderErrorStage::ResponseValidation,
    );
    error
        .validation_errors
        .push("provider_stream_text_mismatch".to_string());
    error
}

fn has_duplicate_tool_call_ids(calls: &[ModelToolCall]) -> bool {
    let mut ids = BTreeSet::new();
    calls
        .iter()
        .any(|call| !ids.insert(call.tool_call_id.as_str()))
}

fn failed_result(error: impl Into<String>) -> AgentLoopResult {
    AgentLoopResult {
        status: AgentStatus::Failed,
        completed: false,
        final_answer: None,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        pending_approvals: Vec::new(),
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

fn provider_error_model_response(
    request: &ModelTurnRequest,
    error: ProviderError,
) -> ModelTurnResponse {
    let provider_capability_metadata = error.capability_metadata.as_deref().cloned();
    let mut response = provider_error_response(request, error);
    response.provider_capability_metadata = provider_capability_metadata;
    response
}

fn merge_provider_attempt_metadata(
    mut previous: ProviderAttemptMetadata,
    current: ProviderAttemptMetadata,
) -> ProviderAttemptMetadata {
    let first_attempt_index = previous.attempt_count.saturating_add(1);
    previous.attempt_count = previous.attempt_count.saturating_add(current.attempt_count);
    previous.retry_count = previous.retry_count.saturating_add(current.retry_count);
    previous.latency_ms = previous.latency_ms.saturating_add(current.latency_ms);
    previous
        .occurrences
        .extend(
            current
                .occurrences
                .into_iter()
                .enumerate()
                .map(|(offset, mut occurrence)| {
                    occurrence.attempt_index = first_attempt_index
                        .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
                    occurrence
                }),
        );
    previous
}

fn merge_model_usage(previous: &mut ModelUsage, current: &ModelUsage) {
    previous.input_tokens = previous.input_tokens.saturating_add(current.input_tokens);
    previous.output_tokens = previous.output_tokens.saturating_add(current.output_tokens);
    previous.total_tokens = previous.total_tokens.saturating_add(current.total_tokens);
    previous.cached_input_tokens = previous
        .cached_input_tokens
        .saturating_add(current.cached_input_tokens);
    previous.reasoning_tokens = previous
        .reasoning_tokens
        .saturating_add(current.reasoning_tokens);
    if let Some(cost) = current.cost_estimate {
        previous.cost_estimate =
            Some(previous.cost_estimate.unwrap_or_default().max(0.0) + cost.max(0.0));
    }
}

fn merge_provider_capability_metadata(
    previous: ProviderCapabilityMetadata,
    mut current: ProviderCapabilityMetadata,
) -> ProviderCapabilityMetadata {
    let mut observations = previous.cache_observations;
    observations.append(&mut current.cache_observations);
    current.cache_observations = observations;
    current.profile_attempts = current
        .profile_attempts
        .saturating_add(previous.profile_attempts);
    current.fallback_count = current
        .fallback_count
        .saturating_add(previous.fallback_count);
    merge_model_usage(&mut current.probe_usage, &previous.probe_usage);
    current.probe_attempt_metadata = merge_provider_attempt_metadata(
        previous.probe_attempt_metadata,
        current.probe_attempt_metadata,
    );
    current
}

fn merge_response_runtime_metadata(response: &mut ModelTurnResponse, earlier: &ProviderError) {
    if let Some(metadata) = earlier.capability_metadata.as_deref() {
        response.provider_capability_metadata =
            Some(match response.provider_capability_metadata.take() {
                Some(current) => merge_provider_capability_metadata(metadata.clone(), current),
                None => metadata.clone(),
            });
    }
    if let Some(metadata) = &earlier.provider_attempt_metadata {
        response.provider_attempt_metadata =
            Some(match response.provider_attempt_metadata.take() {
                Some(current) => merge_provider_attempt_metadata(metadata.clone(), current),
                None => metadata.clone(),
            });
    }
}

fn merge_provider_error_runtime_metadata(error: &mut ProviderError, earlier: &ProviderError) {
    if let Some(metadata) = earlier.capability_metadata.as_deref() {
        error.capability_metadata = Some(Box::new(match error.capability_metadata.take() {
            Some(current) => merge_provider_capability_metadata(metadata.clone(), *current),
            None => metadata.clone(),
        }));
    }
    if let Some(metadata) = &earlier.provider_attempt_metadata {
        error.provider_attempt_metadata = Some(match error.provider_attempt_metadata.take() {
            Some(current) => merge_provider_attempt_metadata(metadata.clone(), current),
            None => metadata.clone(),
        });
    }
}

struct ToolOccurrenceContext {
    identity: OccurrenceIdentity,
    timer: OccurrenceTimer,
    model_turn_ordinal: u32,
    tool_call_ordinal: u32,
    tool_call_id_digest: String,
    tool_name: String,
}

struct ModelToolOccurrence {
    call: ModelToolCall,
    fingerprint: String,
    invalid_was_observed: bool,
    context: ToolOccurrenceContext,
}

struct RuntimeToolResult {
    result: ToolResult,
    duration_ms: Option<u64>,
    event_sink_failed: bool,
}

struct WorkspaceToolExecution {
    output: ToolOutput,
    sandbox_execution: Option<ToolSandboxExecutionObservation>,
    event_sink_failed: bool,
}

struct WorkspaceToolCallContext<'a> {
    bound: &'a BoundToolCall,
    decision: &'a ToolBrokerDecision,
    profile: &'a PermissionProfile,
    occurrence: Option<&'a ToolOccurrenceContext>,
}

struct ObservedToolDecision {
    decision: ToolBrokerDecision,
    cause: PolicyDecisionCause,
}

fn emit_event(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    event: AgentLoopEvent,
) -> Result<(), AgentLoopEventSinkError> {
    match on_event.as_deref_mut() {
        Some(callback) => callback(event),
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_prompt_assembly_finished(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    identity: OccurrenceIdentity,
    timer: &OccurrenceTimer,
    model_turn_ordinal: u32,
    message_count: u32,
    tool_count: u32,
    request_token_count: u32,
    request_digest: String,
    compacted: bool,
    finalization_only: bool,
    status: PromptAssemblyStatus,
) -> Result<(), AgentLoopEventSinkError> {
    emit_event(
        on_event,
        AgentLoopEvent::Observation(AgentObservation::PromptAssembly(
            PromptAssemblyObservation {
                identity,
                lifecycle: timer.finished(status),
                model_turn_ordinal,
                message_count,
                tool_count,
                request_token_count,
                request_digest,
                compacted,
                finalization_only,
            },
        )),
    )
}

fn emit_final_review_finished(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    final_review: &Option<(OccurrenceIdentity, OccurrenceTimer)>,
    model_turn_ordinal: u32,
    status: FinalReviewStatus,
) -> Result<(), AgentLoopEventSinkError> {
    let Some((identity, timer)) = final_review else {
        return Ok(());
    };
    emit_event(
        on_event,
        AgentLoopEvent::Observation(AgentObservation::FinalReview(FinalReviewObservation {
            identity: identity.clone(),
            lifecycle: timer.finished(status),
            model_turn_ordinal,
        })),
    )
}

enum ProviderAttemptIdentityScope {
    Child(OccurrenceIdentity),
    Root {
        thread_id: String,
        turn_id: String,
        model_turn_ordinal: u32,
    },
}

struct ProviderEventBridge<'a, 'callback_ref, 'callback> {
    identity_scope: ProviderAttemptIdentityScope,
    finalization_only: bool,
    on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    streamed_text: String,
    next_attempt_ordinal: u32,
    active_attempt: Option<(ProviderAttemptStarted, OccurrenceIdentity)>,
    event_sink_failed: bool,
}

impl<'a, 'callback_ref, 'callback> ProviderEventBridge<'a, 'callback_ref, 'callback> {
    fn new(
        prompt_identity: OccurrenceIdentity,
        finalization_only: bool,
        on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    ) -> Self {
        Self {
            identity_scope: ProviderAttemptIdentityScope::Child(prompt_identity),
            finalization_only,
            on_event,
            streamed_text: String::new(),
            next_attempt_ordinal: 0,
            active_attempt: None,
            event_sink_failed: false,
        }
    }

    fn new_root(
        input: &AgentLoopInput,
        model_turn_ordinal: u32,
        on_event: &'a mut Option<&'callback_ref mut AgentLoopEventCallback<'callback>>,
    ) -> Self {
        Self {
            identity_scope: ProviderAttemptIdentityScope::Root {
                thread_id: input.thread_id.clone(),
                turn_id: input.turn_id.clone(),
                model_turn_ordinal,
            },
            finalization_only: false,
            on_event,
            streamed_text: String::new(),
            next_attempt_ordinal: 0,
            active_attempt: None,
            event_sink_failed: false,
        }
    }

    fn on_stream(&mut self, event: ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::OutputTextDelta { delta } => {
                self.streamed_text.push_str(&delta);
                if self.finalization_only
                    && emit_event(self.on_event, AgentLoopEvent::FinalTextDelta { delta }).is_err()
                {
                    self.event_sink_failed = true;
                }
            }
        }
    }

    fn on_attempt(&mut self, event: ProviderAttemptEvent) -> bool {
        if self.event_sink_failed {
            return false;
        }
        let result = match event {
            ProviderAttemptEvent::Started(started) => self.start_attempt(started),
            ProviderAttemptEvent::Finished(finished) => self.finish_attempt(finished),
        };
        if result.is_err() {
            self.event_sink_failed = true;
            return false;
        }
        true
    }

    fn start_attempt(&mut self, started: ProviderAttemptStarted) -> Result<(), ()> {
        if self.active_attempt.is_some() {
            return Err(());
        }
        let identity = match &self.identity_scope {
            ProviderAttemptIdentityScope::Child(parent) => {
                child_occurrence_identity(parent, "provider_attempt", self.next_attempt_ordinal)
            }
            ProviderAttemptIdentityScope::Root {
                thread_id,
                turn_id,
                model_turn_ordinal,
            } => root_occurrence_identity(
                thread_id,
                turn_id,
                "provider_attempt",
                *model_turn_ordinal,
                self.next_attempt_ordinal,
            ),
        };
        let observation = ProviderAttemptObservation {
            identity: identity.clone(),
            lifecycle: OccurrenceLifecycle::Started {
                queued_at_unix_ms: started.started_at_unix_ms,
                started_at_unix_ms: started.started_at_unix_ms,
            },
            operation_phase: started.operation_phase,
            provider_name: started.provider_name.clone(),
            model_name: started.model_name.clone(),
            actual_api_protocol: started.actual_api_protocol,
            attempt_index: started.attempt_index,
            retry_count: started.attempt_index.saturating_sub(1),
            request_send_to_headers_ms: None,
            time_to_first_text_delta_ms: None,
            retry_backoff_ms: None,
            error_category: None,
            error_stage: None,
            diagnostic_code: None,
            usage: None,
        };
        emit_event(
            self.on_event,
            AgentLoopEvent::Observation(AgentObservation::ProviderAttempt(Box::new(observation))),
        )
        .map_err(|_| ())?;
        self.active_attempt = Some((started, identity));
        Ok(())
    }

    fn finish_attempt(
        &mut self,
        finished: singularity_model::ProviderAttemptOccurrence,
    ) -> Result<(), ()> {
        let Some((started, identity)) = self.active_attempt.take() else {
            return Err(());
        };
        if started.operation_phase != finished.operation_phase
            || started.provider_name != finished.provider_name
            || started.model_name != finished.model_name
            || started.actual_api_protocol != finished.actual_api_protocol
            || started.attempt_index != finished.attempt_index
            || started.started_at_unix_ms != finished.started_at_unix_ms
        {
            return Err(());
        }
        let status = match finished.terminal_status {
            singularity_model::ProviderAttemptStatus::Ok => ProviderAttemptStatus::Ok,
            singularity_model::ProviderAttemptStatus::Error => ProviderAttemptStatus::Error,
            singularity_model::ProviderAttemptStatus::Cancelled => ProviderAttemptStatus::Cancelled,
        };
        let usage = finished.usage.map(|usage| ProviderAttemptUsageObservation {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        });
        let observation = ProviderAttemptObservation {
            identity,
            lifecycle: OccurrenceLifecycle::Finished {
                queued_at_unix_ms: finished.started_at_unix_ms,
                started_at_unix_ms: finished.started_at_unix_ms,
                ended_at_unix_ms: finished.ended_at_unix_ms,
                duration_ms: finished.attempt_duration_ms,
                status,
            },
            operation_phase: finished.operation_phase,
            provider_name: finished.provider_name,
            model_name: finished.model_name,
            actual_api_protocol: finished.actual_api_protocol,
            attempt_index: finished.attempt_index,
            retry_count: finished.attempt_index.saturating_sub(1),
            request_send_to_headers_ms: finished.request_send_to_headers_ms,
            time_to_first_text_delta_ms: finished.time_to_first_text_delta_ms,
            retry_backoff_ms: finished.retry_backoff_ms,
            error_category: finished.error_category,
            error_stage: finished.error_stage,
            diagnostic_code: finished.diagnostic_code,
            usage,
        };
        emit_event(
            self.on_event,
            AgentLoopEvent::Observation(AgentObservation::ProviderAttempt(Box::new(observation))),
        )
        .map_err(|_| ())?;
        self.next_attempt_ordinal = self.next_attempt_ordinal.saturating_add(1);
        Ok(())
    }
}

fn emit_verification_occurrence(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    input: &AgentLoopInput,
    model_turn_ordinal: u32,
    occurrence_ordinal: u32,
    kind: &str,
    status: VerificationStatus,
    summary: &AgentVerification,
) -> Result<(), AgentLoopEventSinkError> {
    let timer = OccurrenceTimer::start();
    let identity = occurrence_identity(input, kind, model_turn_ordinal, occurrence_ordinal, None);
    for lifecycle in [timer.started(), timer.finished(status)] {
        emit_event(
            on_event,
            AgentLoopEvent::Observation(AgentObservation::Verification(VerificationObservation {
                identity: identity.clone(),
                lifecycle,
                required_command_count: summary.required_command_count,
                satisfied_command_count: summary.satisfied_command_count,
                occurrence_count: occurrence_ordinal,
                command_duration_ms: None,
            })),
        )?;
    }
    Ok(())
}

fn occurrence_identity(
    input: &AgentLoopInput,
    kind: &str,
    model_turn_ordinal: u32,
    ordinal: u32,
    parent_occurrence_id: Option<String>,
) -> OccurrenceIdentity {
    let mut identity = root_occurrence_identity(
        &input.thread_id,
        &input.turn_id,
        kind,
        model_turn_ordinal,
        ordinal,
    );
    identity.parent_occurrence_id = parent_occurrence_id;
    identity
}

fn root_occurrence_identity(
    thread_id: &str,
    turn_id: &str,
    kind: &str,
    model_turn_ordinal: u32,
    ordinal: u32,
) -> OccurrenceIdentity {
    let encoded = format!(
        "{}\u{0}{}\u{0}{kind}\u{0}{model_turn_ordinal}\u{0}{ordinal}",
        thread_id, turn_id
    );
    OccurrenceIdentity {
        occurrence_id: format!("sha256:{:x}", Sha256::digest(encoded.as_bytes())),
        parent_occurrence_id: None,
        ordinal,
    }
}

fn child_occurrence_identity(
    parent: &OccurrenceIdentity,
    kind: &str,
    ordinal: u32,
) -> OccurrenceIdentity {
    let encoded = format!("{}\u{0}{kind}\u{0}{ordinal}", parent.occurrence_id);
    OccurrenceIdentity {
        occurrence_id: format!("sha256:{:x}", Sha256::digest(encoded.as_bytes())),
        parent_occurrence_id: Some(parent.occurrence_id.clone()),
        ordinal,
    }
}

fn tool_occurrence_context(
    input: &AgentLoopInput,
    call: &ModelToolCall,
    model_turn_ordinal: u32,
    tool_call_ordinal: u32,
) -> ToolOccurrenceContext {
    let prompt_parent = occurrence_identity(input, "prompt_assembly", model_turn_ordinal, 0, None);
    ToolOccurrenceContext {
        identity: occurrence_identity(
            input,
            "tool_call",
            model_turn_ordinal,
            tool_call_ordinal,
            Some(prompt_parent.occurrence_id),
        ),
        timer: OccurrenceTimer::start(),
        model_turn_ordinal,
        tool_call_ordinal,
        tool_call_id_digest: format!("sha256:{:x}", Sha256::digest(call.tool_call_id.as_bytes())),
        tool_name: safe_tool_name(call),
    }
}

fn safe_tool_name(call: &ModelToolCall) -> String {
    if call.parse_status == ModelToolParseStatus::Valid
        && !call.tool_name.is_empty()
        && call.tool_name.len() <= 64
        && call
            .tool_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        call.tool_name.clone()
    } else {
        "invalid_tool".to_string()
    }
}

fn tool_call_event(
    context: &ToolOccurrenceContext,
    lifecycle: OccurrenceLifecycle<ToolCallStatus>,
) -> AgentLoopEvent {
    AgentLoopEvent::Observation(AgentObservation::ToolCall(ToolCallObservation {
        identity: context.identity.clone(),
        lifecycle,
        model_turn_ordinal: context.model_turn_ordinal,
        tool_call_ordinal: context.tool_call_ordinal,
        tool_call_id_digest: context.tool_call_id_digest.clone(),
        tool_name: context.tool_name.clone(),
    }))
}

fn emit_rejected_tool_calls(
    on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
    input: &AgentLoopInput,
    calls: &[ModelToolCall],
    model_turn_ordinal: u32,
) -> Result<(), AgentLoopEventSinkError> {
    for (ordinal, call) in calls.iter().enumerate() {
        let context = tool_occurrence_context(
            input,
            call,
            model_turn_ordinal,
            u32::try_from(ordinal).unwrap_or(u32::MAX),
        );
        emit_event(on_event, tool_call_event(&context, context.timer.started()))?;
        emit_event(
            on_event,
            tool_call_event(&context, context.timer.finished(ToolCallStatus::Rejected)),
        )?;
    }
    Ok(())
}

fn tool_result_status(
    prepared: &PreparedToolCall,
    result: &ToolResult,
    batch_rejected: bool,
) -> ToolCallStatus {
    if batch_rejected {
        ToolCallStatus::BatchRejected
    } else if result.failure_kind == Some(ToolFailureKind::Cancelled) {
        ToolCallStatus::Cancelled
    } else if matches!(prepared.decision, Some(ToolBrokerDecision::Deny { .. })) {
        ToolCallStatus::PolicyDenied
    } else if prepared.rejection.is_some() {
        ToolCallStatus::Rejected
    } else if result.ok {
        ToolCallStatus::Succeeded
    } else {
        ToolCallStatus::Failed
    }
}

fn policy_status(decision: &ToolBrokerDecision) -> PolicyDecisionStatus {
    match decision {
        ToolBrokerDecision::Allow | ToolBrokerDecision::Approved { .. } => {
            PolicyDecisionStatus::Allow
        }
        ToolBrokerDecision::Ask { .. } => PolicyDecisionStatus::Ask,
        ToolBrokerDecision::Deny { .. } => PolicyDecisionStatus::Deny,
    }
}

fn tool_operation_count(bound: &BoundToolCall, profile: &PermissionProfile) -> u32 {
    if matches!(
        bound.executor,
        ToolExecutor::Workspace(WorkspaceToolExecutor::Command)
    ) && profile.network_access == NetworkAccess::Allowed
    {
        2
    } else {
        1
    }
}

fn safe_policy_cause(cause: &PermissionCause) -> PolicyDecisionCause {
    match cause {
        PermissionCause::Explicit => PolicyDecisionCause::Explicit,
        PermissionCause::Rule => PolicyDecisionCause::Rule,
        PermissionCause::FilesystemProfile => PolicyDecisionCause::FilesystemProfile,
        PermissionCause::NetworkProfile => PolicyDecisionCause::NetworkProfile,
        PermissionCause::ProtectedResource => PolicyDecisionCause::ProtectedResource,
        PermissionCause::NoMatchingRule => PolicyDecisionCause::NoMatchingRule,
        PermissionCause::ApprovalPolicy => PolicyDecisionCause::ApprovalPolicy,
    }
}

fn sandbox_status(status: ToolSandboxExecutionStatus) -> SandboxExecutionStatus {
    match status {
        ToolSandboxExecutionStatus::Ok => SandboxExecutionStatus::Ok,
        ToolSandboxExecutionStatus::Error => SandboxExecutionStatus::Error,
        ToolSandboxExecutionStatus::TimedOut => SandboxExecutionStatus::TimedOut,
        ToolSandboxExecutionStatus::Cancelled => SandboxExecutionStatus::Cancelled,
    }
}

fn sandbox_boundary_event(
    occurrence: &ToolOccurrenceContext,
    boundary: ToolSandboxExecutionBoundary,
) -> AgentLoopEvent {
    let identity = child_occurrence_identity(&occurrence.identity, "sandbox_execution", 0);
    let observation = match boundary {
        ToolSandboxExecutionBoundary::Started {
            command_id,
            started_at_unix_ms,
        } => SandboxExecutionOccurrence {
            identity,
            lifecycle: OccurrenceLifecycle::Started {
                queued_at_unix_ms: started_at_unix_ms,
                started_at_unix_ms,
            },
            command_id,
            command_id_binding_valid: None,
            workspace_mutation: None,
            enforcement: None,
        },
        ToolSandboxExecutionBoundary::Finished(sandbox) => SandboxExecutionOccurrence {
            identity,
            lifecycle: OccurrenceLifecycle::Finished {
                queued_at_unix_ms: sandbox.started_at_unix_ms,
                started_at_unix_ms: sandbox.started_at_unix_ms,
                ended_at_unix_ms: sandbox.ended_at_unix_ms,
                duration_ms: sandbox.duration_ms,
                status: sandbox_status(sandbox.status),
            },
            command_id: sandbox.command_id,
            command_id_binding_valid: Some(sandbox.command_id_binding_valid),
            workspace_mutation: Some(sandbox.workspace_mutation),
            enforcement: Some(sandbox.enforcement),
        },
    };
    AgentLoopEvent::Observation(AgentObservation::SandboxExecution(observation))
}

fn safe_request_digest(request: &ModelTurnRequest) -> String {
    let encoded = serde_json::to_vec(request).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(encoded))
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
    // 在内部 occurrence 中保留拒绝调用的诊断信息，但不把 provider 原始名称或参数重放到下一次请求。
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
    prepared: &PreparedToolCall,
    reason: &str,
) -> ApprovalRequest {
    let call = &prepared.call;
    let bound = prepared
        .bound
        .as_ref()
        .expect("approval requires a registry-bound tool call");
    let mut request = ApprovalRequest::new(
        approval_request_id,
        input.thread_id.clone(),
        input.turn_id.clone(),
        bound.tool_id.clone(),
    )
    .with_tool_call_id(call.tool_call_id.clone())
    .with_resources(bound.resources.clone());
    request.reason = reason.to_string();
    request
}

fn approval_request_id(input: &AgentLoopInput, call: &ModelToolCall) -> String {
    approval_request_id_from_tool_call_id(&input.turn_id, &call.tool_call_id)
}

fn approval_request_id_from_tool_call_id(turn_id: &str, tool_call_id: &str) -> String {
    format!("approval_{}_{}", turn_id, tool_call_id)
}

fn canonicalize_model_tool_call(
    tool_broker: &ToolBroker,
    model_call: &ModelToolCall,
) -> Result<ModelToolCall, String> {
    let (_, execution_arguments) = tool_broker
        .prepare_model_input(&model_call.tool_name, &model_call.arguments)
        .map_err(|error| error.code)?;
    let mut bound_call = model_call.clone();
    bound_call.raw_arguments = execution_arguments.to_string();
    bound_call.arguments = execution_arguments;
    tool_broker
        .validate_execution_input(&bound_call.tool_name, &bound_call.arguments)
        .map_err(|error| error.code)?;
    Ok(bound_call)
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

fn permission_failure_kind(cause: &PermissionCause) -> ToolFailureKind {
    match cause {
        PermissionCause::FilesystemProfile => ToolFailureKind::PermissionProfile,
        PermissionCause::NetworkProfile => ToolFailureKind::PermissionProfile,
        PermissionCause::ProtectedResource => ToolFailureKind::ProtectedPath,
        PermissionCause::ApprovalPolicy => ToolFailureKind::Approval,
        PermissionCause::Explicit | PermissionCause::Rule | PermissionCause::NoMatchingRule => {
            ToolFailureKind::Policy
        }
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
        AgentLoopToolError::Workspace(WorkspaceToolError::ObservationSinkFailed) => {
            (ToolFailureKind::Infrastructure, "event_sink_failed")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::Cancelled) => {
            (ToolFailureKind::Cancelled, "tool_cancelled")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::BinaryPattern) => {
            (ToolFailureKind::Execution, "binary_pattern")
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ConcurrentMutation(_)) => (
            ToolFailureKind::Infrastructure,
            "workspace_concurrent_mutation",
        ),
        AgentLoopToolError::Workspace(WorkspaceToolError::HardLinkRejected(_)) => (
            ToolFailureKind::WorkspaceBoundary,
            "workspace_hard_link_rejected",
        ),
        AgentLoopToolError::Workspace(WorkspaceToolError::PathIdentityUnsupported(_)) => (
            ToolFailureKind::Capability,
            "workspace_identity_unsupported",
        ),
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
        AgentLoopToolError::Workspace(WorkspaceToolError::ObservationSinkFailed) => {
            EVENT_SINK_FAILURE_ERROR
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::Cancelled) => "tool execution cancelled",
        AgentLoopToolError::Workspace(WorkspaceToolError::RollbackFailed(_)) => {
            "workspace rollback failed"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ConcurrentMutation(_)) => {
            "workspace target changed during execution"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::HardLinkRejected(_)) => {
            "workspace hard-linked file is not trusted"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::PathIdentityUnsupported(_)) => {
            "workspace object identity cannot be verified"
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
mod context_accounting_tests {
    use super::*;

    fn tool_message(tool_call_id: &str, content: &str) -> ModelMessage {
        let mut message = ModelMessage::text(ModelRole::Tool, content);
        message.tool_call_id = Some(tool_call_id.to_string());
        message
    }

    fn assistant_tool_message(tool_call_ids: &[&str]) -> ModelMessage {
        ModelMessage::assistant_tool_calls(
            tool_call_ids
                .iter()
                .map(|tool_call_id| ModelToolCall {
                    tool_call_id: (*tool_call_id).to_string(),
                    tool_name: TOOL_COMMAND.to_string(),
                    arguments: json!({}),
                    raw_arguments: "{}".to_string(),
                    parse_status: ModelToolParseStatus::Valid,
                    validation_errors: Vec::new(),
                })
                .collect(),
        )
    }

    #[test]
    fn tool_result_accounting_uses_occurrence_order_for_duplicate_ids_and_placeholders() {
        let mut hidden = ToolResult::summary("same_call", TOOL_COMMAND, false, "approval");
        hidden.failure_kind = Some(ToolFailureKind::Approval);
        let occurrences = vec![
            ToolResultOccurrence::new(
                ToolResult::summary("same_call", TOOL_COMMAND, true, "first")
                    .with_context_token_count(100),
                ToolResultVisibility::Visible,
            ),
            ToolResultOccurrence::new(
                hidden.with_context_token_count(200),
                ToolResultVisibility::Hidden,
            ),
            ToolResultOccurrence::new(
                ToolResult::summary("same_call", TOOL_COMMAND, true, "second")
                    .with_context_token_count(20),
                ToolResultVisibility::Visible,
            ),
            ToolResultOccurrence::new(
                ToolResult::summary("same_call", TOOL_COMMAND, true, "compacted")
                    .with_context_token_count(300),
                ToolResultVisibility::Compacted,
            ),
            ToolResultOccurrence::new(
                ToolResult::summary("other_call", TOOL_COMMAND, true, "other compacted")
                    .with_context_token_count(400),
                ToolResultVisibility::Compacted,
            ),
        ];
        let compacted_message = compacted_tool_result_message(
            &tool_message("same_call", "original tool message"),
            occurrences.get(3),
        );
        let other_compacted_message = compacted_tool_result_message(
            &tool_message("other_call", "original other tool message"),
            occurrences.get(4),
        );
        let messages = vec![
            assistant_tool_message(&["same_call"]),
            tool_message("same_call", "first public payload"),
            assistant_tool_message(&["same_call"]),
            assistant_tool_message(&["same_call"]),
            tool_message("same_call", "second public payload"),
            assistant_tool_message(&["same_call", "other_call"]),
            compacted_message,
            other_compacted_message,
        ];
        let expected = 100u32
            .saturating_sub(approximate_token_count("first public payload"))
            .saturating_add(20u32.saturating_sub(approximate_token_count("second public payload")));

        assert_eq!(
            tool_result_context_token_adjustment(&messages, &occurrences),
            expected
        );
    }

    #[test]
    fn tool_result_occurrence_rejects_visible_result_marked_compacted() {
        let occurrences = vec![ToolResultOccurrence::new(
            ToolResult::summary("call_1", TOOL_COMMAND, true, "safe public payload"),
            ToolResultVisibility::Compacted,
        )];
        let messages = vec![
            assistant_tool_message(&["call_1"]),
            tool_message("call_1", "safe public payload"),
        ];
        assert!(
            tool_result_message_occurrences(&messages, &occurrences).is_none(),
            "a normal tool payload cannot satisfy a compacted checkpoint binding"
        );
        assert_eq!(
            tool_result_context_token_adjustment(&messages, &occurrences),
            u32::MAX
        );
    }

    #[test]
    fn checkpoint_rebuilds_only_full_safe_small_result_accounting() {
        let envelope = ToolCallRequest::new("call_1", TOOL_COMMAND, "{}");
        let result = ToolResult::from_result(
            &envelope,
            &ToolOutput::success(json!({"summary": "small-safe-output"})),
        );
        let expected = result
            .context_token_count()
            .expect("safe result accounting");
        let mut legacy = ToolResultOccurrenceWire {
            result: result.clone(),
            visibility: None,
            result_id: result.result_id.clone(),
            context_token_count: result.context_token_count(),
            audit_metadata: result.audit_metadata().cloned(),
            workspace_observation: result.workspace_observation().cloned(),
        };
        legacy.context_token_count = None;
        let restored = ToolResultOccurrence::from_wire(legacy, ToolResultVisibility::Visible)
            .expect("small safe legacy result remains compatible");
        assert_eq!(restored.result().context_token_count(), Some(expected));

        let mut inconsistent = ToolResultOccurrenceWire {
            result: result.clone(),
            visibility: None,
            result_id: result.result_id.clone(),
            context_token_count: result.context_token_count(),
            audit_metadata: result.audit_metadata().cloned(),
            workspace_observation: result.workspace_observation().cloned(),
        };
        inconsistent.context_token_count = Some(expected.saturating_add(1));
        assert!(
            ToolResultOccurrence::from_wire(inconsistent, ToolResultVisibility::Visible).is_err()
        );

        let large = ToolResult::from_result(
            &envelope,
            &ToolOutput::success(json!({"stdout": "large-safe-output".repeat(2_000)})),
        );
        let mut untrusted_legacy = ToolResultOccurrenceWire {
            result: large.clone(),
            visibility: None,
            result_id: large.result_id.clone(),
            context_token_count: large.context_token_count(),
            audit_metadata: large.audit_metadata().cloned(),
            workspace_observation: large.workspace_observation().cloned(),
        };
        untrusted_legacy.context_token_count = None;
        assert!(
            ToolResultOccurrence::from_wire(untrusted_legacy, ToolResultVisibility::Visible)
                .is_err()
        );
    }
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
    fn terminal_command_observations_filter_failures_and_pre_mutation_results() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut failed = ToolResult::summary("failed", TOOL_COMMAND, false, "failed");
        failed.result_id = Some(digest.to_string());
        failed = failed.with_workspace_observation(WorkspaceObservation::unchanged(
            WorkspaceRevision::initial(),
        ));
        let mut before_mutation = ToolResult::summary("before", TOOL_COMMAND, true, "ok");
        before_mutation.result_id = Some(digest.to_string());
        before_mutation = before_mutation.with_workspace_observation(
            WorkspaceObservation::unchanged(WorkspaceRevision::initial()),
        );
        let mutation = ToolResult::summary("edit", TOOL_EDIT, true, "changed")
            .with_workspace_observation(WorkspaceObservation::changed(
                WorkspaceRevision::initial().next().expect("revision"),
            ));
        let mut after_mutation = ToolResult::summary("after", TOOL_COMMAND, true, "ok");
        after_mutation.result_id = Some(digest.to_string());
        after_mutation = after_mutation.with_workspace_observation(
            WorkspaceObservation::unchanged(WorkspaceRevision::initial().next().expect("revision")),
        );

        assert_eq!(
            terminal_command_scope_digests(&[failed, before_mutation, mutation, after_mutation], 1),
            vec![digest.to_string()]
        );
    }

    #[test]
    fn terminal_command_observations_preserve_revision_across_unexecuted_failures() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let mutation = ToolResult::summary("edit", TOOL_EDIT, true, "changed")
            .with_workspace_observation(WorkspaceObservation::changed(revision));
        let rejected = ToolResult::summary("invalid", TOOL_COMMAND, false, "invalid arguments");
        let mut verified = ToolResult::summary("verified", TOOL_COMMAND, true, "ok");
        verified.result_id = Some(digest.to_string());
        verified = verified.with_workspace_observation(WorkspaceObservation::unchanged(revision));

        assert_eq!(
            terminal_command_scope_digests(&[mutation, rejected, verified], 1),
            vec![digest.to_string()]
        );
    }

    #[test]
    fn completion_gate_requires_exact_successful_command_observation_after_mutation() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut tracker = CompletionTracker::default();
        let revision = WorkspaceRevision::initial().next().expect("revision");
        tracker.observe(
            &ToolResult::summary("edit", TOOL_EDIT, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision)),
        );

        let mut failed = ToolResult::summary("failed", TOOL_COMMAND, false, "failed");
        failed.result_id = Some(digest.to_string());
        failed = failed.with_workspace_observation(WorkspaceObservation::unchanged(revision));
        tracker.observe(&failed);
        assert!(!tracker.verification_satisfied());

        let mut malformed = ToolResult::summary("malformed", TOOL_COMMAND, true, "ok");
        malformed.result_id = Some("sha256:not-a-digest".to_string());
        malformed = malformed.with_workspace_observation(WorkspaceObservation::unchanged(revision));
        tracker.observe(&malformed);
        assert!(!tracker.verification_satisfied());

        let mut successful = ToolResult::summary("successful", TOOL_COMMAND, true, "ok");
        successful.result_id = Some(digest.to_string());
        successful =
            successful.with_workspace_observation(WorkspaceObservation::unchanged(revision));
        tracker.observe(&successful);
        assert!(tracker.verification_satisfied());
    }

    #[test]
    fn exact_completion_requires_required_commands_as_the_terminal_multiset() {
        let required = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut tracker =
            CompletionTracker::from_requirements(&[AgentVerificationRequirement::new(required, 2)])
                .expect("completion tracker");
        let command = |call_id: &str, digest: &str, revision: WorkspaceRevision| {
            let mut result = ToolResult::summary(call_id, TOOL_COMMAND, true, "ok");
            result.result_id = Some(digest.to_string());
            result.with_workspace_observation(WorkspaceObservation::unchanged(revision))
        };
        let initial = WorkspaceRevision::initial();
        let changed = initial.next().expect("revision");

        tracker.observe(&command("smoke-1", required, initial));
        tracker.observe(&command("smoke-2", required, initial));
        assert!(tracker.verification_satisfied());

        tracker.observe(
            &ToolResult::summary("write-after-smoke", TOOL_COMMAND, true, "ok")
                .with_workspace_observation(WorkspaceObservation::changed(changed)),
        );
        assert!(!tracker.verification_satisfied());
        tracker.observe(&command("smoke-3", required, changed));
        assert!(!tracker.verification_satisfied());
        tracker.observe(&command("smoke-4", required, changed));
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

        let envelope = ToolCallRequest::new("call_1", singularity_tools::READ_TOOL, "{}");
        let result = ToolResult::from_result(&envelope, &output);
        assert!(!result.ok);
        assert_eq!(result.error_code.as_deref(), Some("tool_cancelled"));
        assert_eq!(result.failure_kind, Some(ToolFailureKind::Cancelled));
        assert_eq!(result.to_message_payload()["ok"], false);
    }

    #[test]
    fn workspace_identity_failures_map_to_stable_safe_tool_results() {
        let cases = [
            (
                WorkspaceToolError::ConcurrentMutation("secret-target".to_string()),
                ToolFailureKind::Infrastructure,
                "workspace_concurrent_mutation",
                "workspace target changed during execution",
            ),
            (
                WorkspaceToolError::HardLinkRejected("secret-target".to_string()),
                ToolFailureKind::WorkspaceBoundary,
                "workspace_hard_link_rejected",
                "workspace hard-linked file is not trusted",
            ),
            (
                WorkspaceToolError::PathIdentityUnsupported("secret-target".to_string()),
                ToolFailureKind::Capability,
                "workspace_identity_unsupported",
                "workspace object identity cannot be verified",
            ),
        ];

        for (error, expected_kind, expected_code, expected_summary) in cases {
            let output = workspace_tool_failure(AgentLoopToolError::Workspace(error));
            assert!(!output.ok);
            assert_eq!(output.failure_kind, Some(expected_kind));
            assert_eq!(output.error_code.as_deref(), Some(expected_code));
            assert_eq!(output.content["summary"], expected_summary);
            assert!(!output.content.to_string().contains("secret-target"));
        }
    }

    #[test]
    fn late_success_is_replaced_by_cancellation_result() {
        let call = ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: singularity_tools::READ_TOOL.to_string(),
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
