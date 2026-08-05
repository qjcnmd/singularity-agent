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
    PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider, ProviderAttemptMetadata,
    ProviderCapabilityMetadata, ProviderDiagnostic, ProviderError, ProviderErrorStage,
    ProviderProtocolContract, ProviderReasoningReplay, is_strict_tool_schema_compatible,
    provider_error_response, validate_model_request_with_capabilities,
    validate_model_turn_response,
};
use singularity_policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, NetworkAccess, PermissionDecision,
    PermissionDecisionCause as PermissionCause, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionProfileName, PermissionRequest, PermissionResource, PolicyEngine,
    ToolId,
};
use singularity_tools::{
    BoundToolCall, COMMAND_TOOL as TOOL_COMMAND, CommandToolInput, GrepToolInput, ListToolInput,
    PATCH_TOOL as TOOL_PATCH, ReadToolInput,
    SandboxExecutionSinkError as ToolSandboxExecutionSinkError, SandboxFilesystemMode,
    SandboxNetworkMode, ToolBroker, ToolBrokerDecision, ToolExecutionMode, ToolExecutor,
    ToolFailureKind, ToolInputValidationError, ToolOutput, ToolResult, ToolSpec, WorkspaceMutation,
    WorkspacePatch, WorkspaceRevision, WorkspaceToolError, WorkspaceToolExecutor, WorkspaceTools,
    approximate_token_count, command_script_scope_digest_with_policy,
};
use thiserror::Error;

mod checkpoint;
mod completion;
mod context;
mod model_turn;
mod observation;
mod tool_occurrence;

use checkpoint::HistoricalModelContext;

pub use checkpoint::{
    ApprovalCheckpoint, PendingApprovalOccurrence, PendingToolCall, TurnCheckpoint,
    TurnCheckpointEvent, TurnCheckpointPhase,
};

impl TurnCheckpoint {
    /// Append accepted user messages and conservatively invalidate decisions made before them.
    ///
    /// When a validated tool call has not started, `cancelled_tool_call` closes the structured
    /// assistant call with a typed, model-visible failure before the new user message is added.
    pub fn with_user_inputs(
        mut self,
        inputs: &[String],
        cancel_pending_tool_calls: bool,
    ) -> Result<Self, String> {
        if inputs.is_empty() || inputs.iter().any(|input| input.trim().is_empty()) {
            return Err("turn checkpoint user input is empty".to_string());
        }
        if cancel_pending_tool_calls {
            self = self.with_pending_tool_failure(
                "not_executed_due_to_user_input",
                "tool was not executed because newer user input changed the task",
                false,
            )?;
        }
        self.append_user_inputs(inputs)
    }

    fn with_pending_tool_failure(
        mut self,
        error_code: &str,
        summary: &str,
        observe_completion: bool,
    ) -> Result<Self, String> {
        if self.pending_tool_calls.is_empty() {
            return Err("turn checkpoint pending tool call is missing".to_string());
        }
        for pending in &self.pending_tool_calls {
            let tool_call_id = pending.tool_call_id.as_str();
            let tool_name = pending.tool_name.as_str();
            let call_is_present = self.state.messages.iter().rev().any(|message| {
                message.role == ModelRole::Assistant
                    && message.tool_calls.iter().any(|call| {
                        call.tool_call_id == tool_call_id && call.tool_name == tool_name
                    })
            });
            if !call_is_present {
                return Err("turn checkpoint pending tool call is missing".to_string());
            }
            let mut result = ToolResult::summary(tool_call_id, tool_name, false, summary);
            result.error_code = Some(error_code.to_string());
            result.failure_kind = Some(ToolFailureKind::Cancelled);
            if observe_completion {
                self.state.completion.observe(&result);
            }
            self.state.messages.push(tool_result_message(&result));
            self.state
                .tool_result_occurrences
                .push(ToolResultOccurrence::new(
                    result.clone(),
                    ToolResultVisibility::Visible,
                ));
        }
        self.pending_tool_calls.clear();
        Ok(self)
    }

    fn append_user_inputs(mut self, inputs: &[String]) -> Result<Self, String> {
        self.state.completion.invalidate_after_user_input();
        self.state.messages.extend(
            inputs
                .iter()
                .map(|input| ModelMessage::text(ModelRole::User, input.clone())),
        );
        self.state.repair_state = None;
        self.state.last_completion_error = None;
        self.state.last_repair_failure = None;
        self.encode().map(|_| self)
    }
}

impl ApprovalCheckpoint {
    /// Convert a superseded approval boundary into the ordinary turn continuation authority.
    ///
    /// A steer closes the approved-but-not-started call with a typed ToolResult. A pause keeps the
    /// canonical pending call so resume can revalidate it against current capabilities and policy.
    pub fn into_turn_checkpoint(
        &self,
        inputs: &[String],
        cancel_pending_tool_call: bool,
    ) -> Result<TurnCheckpoint, String> {
        if cancel_pending_tool_call == inputs.is_empty() {
            return Err("approval checkpoint input handoff is inconsistent".to_string());
        }
        let mut state = self.state.clone();
        state.checkpoint_version = TURN_CHECKPOINT_VERSION;
        let checkpoint = TurnCheckpoint {
            state,
            pending_tool_calls: vec![self.pending_tool_call.clone()],
        };
        if cancel_pending_tool_call {
            checkpoint.with_user_inputs(inputs, true)
        } else {
            checkpoint.encode().map(|_| checkpoint)
        }
    }

    /// Convert a denied, not-yet-started approval into an ordinary continuation boundary.
    pub fn into_turn_checkpoint_after_denial(
        &self,
        inputs: &[String],
    ) -> Result<TurnCheckpoint, String> {
        if inputs.iter().any(|input| input.trim().is_empty()) {
            return Err("approval checkpoint input handoff is inconsistent".to_string());
        }
        let mut state = self.state.clone();
        state.checkpoint_version = TURN_CHECKPOINT_VERSION;
        let checkpoint = TurnCheckpoint {
            state,
            pending_tool_calls: vec![self.pending_tool_call.clone()],
        }
        .with_pending_tool_failure(
            "approval_denied",
            "tool was not executed because approval was denied",
            true,
        )?;
        if inputs.is_empty() {
            checkpoint.encode().map(|_| checkpoint)
        } else {
            checkpoint.append_user_inputs(inputs)
        }
    }
}

/// Consumer callback for durable turn-boundary persistence.
pub type AgentLoopCheckpointCallback<'a> =
    dyn FnMut(TurnCheckpointEvent) -> Result<(), AgentLoopEventSinkError> + 'a;
use completion::{
    CompletionTracker, RepairFailureState, ToolResultOccurrence, ToolResultVisibility,
};
pub use completion::{successful_command_scope_digest, terminal_command_scope_digests};
pub use context::{
    AgentContextItem, AgentContextItemPriority, AgentContextTrace, ContextBundle,
    assemble_context_items,
};
use context::{ContextBudget, assemble_context_items_with_budget, current_turn_excluded};
use model_turn::*;
use observation::OccurrenceTimer;
pub use observation::{
    AgentLoopEvent, AgentLoopEventCallback, AgentLoopEventSinkError, AgentObservation,
    OccurrenceIdentity, OccurrenceLifecycle, PolicyDecisionCause, PolicyDecisionObservation,
    PolicyDecisionStatus, PromptAssemblyObservation, PromptAssemblyStatus,
    ProviderAttemptObservation, ProviderAttemptStatus, ProviderAttemptUsageObservation,
    SandboxExecutionOccurrence, SandboxExecutionStatus, ToolCallObservation, ToolCallStatus,
    VerificationObservation, VerificationStatus,
};
use tool_occurrence::*;

#[cfg(test)]
use completion::ToolResultOccurrenceWire;
#[cfg(test)]
use singularity_tools::{ToolCallRequest, WorkspaceObservation};

const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 16;
const MAX_PARALLEL_READ_TOOL_CALLS: u32 = 8;
/// Approval checkpoints after removing the latest-only mutation summary.
const APPROVAL_CHECKPOINT_VERSION: u32 = 6;
/// Ordinary turn checkpoints after removing the latest-only mutation summary.
const TURN_CHECKPOINT_VERSION: u32 = 5;
const AGENT_DEVELOPER_INSTRUCTIONS: &str = "You are a coding agent working in the current workspace. Inspect real files before making claims. Use tools for changes, write only inside the workspace, and run verification after the last mutation. Report only completed work and verification. Read-only questions need no changes or verification. For multi-step work, keep a concise private checklist; update it when evidence or failure changes the approach, and complete the requested work before the final answer. Tools can be submitted only through native structured tool calls; ordinary text is never executed. Match registered tool schemas exactly and use typed tool results to correct parameters.";
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
const MAX_REPAIR_ATTEMPTS: u32 = 3;
const MAX_REPAIR_CONTEXT_CHARS: usize = 512;
const MAX_REPAIR_CONTEXT_SERIALIZED_CHARS: usize = 65_536;
const MAX_BOUNDED_TEXT_CHARS: usize = 512;
const REPEATED_FAILURE_RECOVERY_INSTRUCTIONS: &str = "The same repairable tool failure recurred. Read the registered tool schema and the previous tool result, then choose a different next action. Do not repeat the same call.";
const REPAIR_STATE_INSTRUCTIONS: &str = "Follow the bounded repair guidance. Use the latest typed tool result and trusted workspace revision evidence to choose the next valid action. When a repair strategy change is required, make a materially different workspace mutation that addresses the reported failure before retrying verification. Do not claim success without new verification evidence.";
const COMPLETION_REPAIR_SIGNATURE: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000017";
const EVENT_SINK_FAILURE_ERROR: &str = "agent event sink failed";

/// 一次 `AgentLoop` 运行的外部可观察生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Paused,
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
            Self::Paused => "paused",
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
            "paused" => Self::Paused,
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

/// Why a bounded repair was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentRepairReason {
    VerificationFailed,
    ToolFailure,
    RevisionConflict,
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
    /// Construct a non-terminal pause result; the durable checkpoint remains authoritative.
    pub fn paused() -> Self {
        let mut status = Self::failed("turn paused at a durable boundary");
        status.status = AgentStatus::Paused;
        status.error = None;
        status
    }

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
    resume_attempt: u32,
    #[serde(skip)]
    #[schemars(skip)]
    provider_reasoning_history: Vec<ProviderReasoningReplay>,
    #[serde(skip)]
    #[schemars(skip)]
    historical_checkpoint: Option<HistoricalModelContext>,
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
            resume_attempt: 0,
            provider_reasoning_history: Vec::new(),
            historical_checkpoint: None,
        }
    }

    /// Bind the durable continuation epoch used by runtime occurrence identities.
    pub fn with_resume_attempt(mut self, resume_attempt: u32) -> Self {
        self.resume_attempt = resume_attempt;
        self
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

    /// Bind opaque provider reasoning history restored from the private checkpoint boundary.
    pub fn with_provider_reasoning_history(
        mut self,
        history: impl IntoIterator<Item = ProviderReasoningReplay>,
    ) -> Self {
        self.provider_reasoning_history = history.into_iter().collect();
        self
    }

    /// Bind the complete cumulative model history of the most recent completed turn.
    ///
    /// When present, this seed is the only historical channel for a fresh turn: public
    /// `with_history` items must not be mixed in (the app-server switches to this
    /// constructor for fresh `turn/start`).
    pub fn with_historical_checkpoint(mut self, checkpoint: &TurnCheckpoint) -> Self {
        self.historical_checkpoint = Some(HistoricalModelContext::from_checkpoint(checkpoint));
        self
    }
}

/// Compaction summary 消息的固定 JSON 前缀，跨轮 seed 保留该 Developer 消息。
const COMPACTION_SUMMARY_PREFIX: &str = "{\"type\":\"agent_context_compaction\"";

/// completion feedback Developer 消息的固定前缀（见 `CompletionTracker::feedback`）。
const COMPLETION_FEEDBACK_PREFIX: &str = "Do not finalize yet.";

/// repair 预算耗尽提示（防御性剔除；正常不出现于 completed turn）。
const REPAIR_BUDGET_EXHAUSTED_INSTRUCTION: &str =
    "repair budget exhausted; refusing another repair attempt";

/// 从跨轮 seed 组装新 Turn 的初始模型消息。
///
/// 规则：删除开头连续的旧 leading Developer；剔除 repair/repeated-failure/budget/
/// completion 瞬态 Developer（全部为固定常量匹配，不按自由文本猜测）；保留
/// compaction summary（固定 JSON 前缀）；插入当前唯一 leading block；末尾追加
/// 当前 user 输入。
fn prepare_seed_messages(
    seed: &HistoricalModelContext,
    input: &AgentLoopInput,
    max_tool_calls: u32,
    current_user_text: &str,
) -> Vec<ModelMessage> {
    let mut messages = Vec::with_capacity(seed.messages.len() + 2);
    let mut seed_messages = seed.messages.iter().cloned();
    for message in seed_messages.by_ref() {
        if message.role != ModelRole::Developer
            || message.content.starts_with(COMPACTION_SUMMARY_PREFIX)
        {
            // 旧 leading block 被替换；compaction summary 紧跟 leading 且必须保留。
            messages.push(message);
            break;
        }
    }
    for message in seed_messages {
        if message.role == ModelRole::Developer
            && !message.content.starts_with(COMPACTION_SUMMARY_PREFIX)
            && (message.content.starts_with(REPAIR_STATE_INSTRUCTIONS)
                || message.content == REPEATED_FAILURE_RECOVERY_INSTRUCTIONS
                || message.content == REPAIR_BUDGET_EXHAUSTED_INSTRUCTION
                || message.content.starts_with(COMPLETION_FEEDBACK_PREFIX))
        {
            continue;
        }
        messages.push(message);
    }
    messages.insert(
        0,
        ModelMessage::text(
            ModelRole::Developer,
            developer_instructions(input, max_tool_calls),
        ),
    );
    messages.push(ModelMessage::text(ModelRole::User, current_user_text));
    messages
}

/// 从输入中提取当前 turn 的用户文本（seed 路径使用）。
fn current_user_text_from_input(input: &AgentLoopInput) -> String {
    input
        .input
        .iter()
        .filter(|item| item.role == USER_MESSAGE_ROLE)
        .map(|item| item.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 网络前 replay 兼容性预检查：在 capability probe 与 Initial checkpoint 之前拒绝
/// 无法证明兼容的私有 replay。不调用 provider、不发网络；transport 层的严格校验
/// （`validate_reasoning_history`）仍为最终防线。
fn validate_replay_preflight(input: &AgentLoopInput) -> Result<(), String> {
    let mut replays = input.provider_reasoning_history.iter().collect::<Vec<_>>();
    if let Some(seed) = input.historical_checkpoint.as_ref() {
        replays.extend(seed.provider_reasoning_history.iter());
    }
    if replays.is_empty() {
        return Ok(());
    }
    let selector = input.model_preferences.model_name.as_deref();
    let selector_provider = selector
        .and_then(|selector| selector.split_once('/'))
        .map(|(provider, _)| provider);
    // selector 的 model 分量：`provider/model#effort` 中 `#` 前的部分。
    let selector_model = selector
        .and_then(|selector| selector.split_once('/'))
        .and_then(|(_, model_and_effort)| model_and_effort.split_once('#'))
        .map(|(model, _)| model)
        .or_else(|| {
            selector
                .and_then(|selector| selector.split_once('/'))
                .map(|(_, rest)| rest)
        });
    let selector_effort = selector
        .and_then(|selector| selector.rsplit_once('#'))
        .map(|(_, effort)| effort);
    for replay in replays {
        let (provider_name, model_name, reasoning_effort, tool_call_ids) = match replay {
            ProviderReasoningReplay::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                ..
            } => (
                provider_name.as_str(),
                model_name.as_str(),
                reasoning_effort.as_str(),
                tool_call_ids,
            ),
            ProviderReasoningReplay::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                ..
            } => (
                provider_name.as_str(),
                model_name.as_str(),
                reasoning_effort.as_str(),
                tool_call_ids,
            ),
        };
        if provider_name.is_empty() || model_name.is_empty() || reasoning_effort.is_empty() {
            return Err("provider reasoning replay is missing identity metadata".to_string());
        }
        if tool_call_ids.is_empty() {
            return Err("provider reasoning replay is missing its tool call binding".to_string());
        }
        if let Some(expected_provider) = selector_provider
            && provider_name != expected_provider
        {
            return Err(format!(
                "provider reasoning replay from {provider_name} cannot be replayed by the resolved provider {expected_provider}"
            ));
        }
        if let Some(expected_model) = selector_model
            && model_name != expected_model
        {
            return Err(format!(
                "provider reasoning replay for {model_name} cannot be replayed by the resolved model {expected_model}"
            ));
        }
        if let Some(expected_effort) = selector_effort
            && reasoning_effort != expected_effort
        {
            return Err(format!(
                "provider reasoning replay effort {reasoning_effort} does not match the resolved selector effort {expected_effort}"
            ));
        }
    }
    Ok(())
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
    provider_reasoning_history: Vec<ProviderReasoningReplay>,
    tool_result_occurrences: Vec<ToolResultOccurrence>,
    pending_approvals: Vec<PendingApprovalOccurrence>,
    used_approval_grants: BTreeSet<String>,
    prior_approval_count: u32,
    completion: CompletionTracker,
    repair_state: Option<RepairState>,
    /// Monotonic repair-attempt ledger for the current episode. The active state may be cleared
    /// after real progress, but this counter survives that transition and approval checkpoints.
    repair_attempts: u32,
    repair_cycles: Vec<RepairCycleRecord>,
    last_completion_error: Option<String>,
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

/// Internal repair state. The signature is a hash-only identity used to bound repeated repair;
/// raw failure text and tool arguments never cross the checkpoint or trace projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairState {
    reason: AgentRepairReason,
    attempt: u32,
    max_attempts: u32,
    required_revision: Option<WorkspaceRevision>,
    signature: String,
    /// The tool whose legal success can close a tool-failure episode.  Other repair reasons
    /// require mutation or required verification progress instead.
    failed_tool_name: Option<String>,
}

/// Producer-owned evidence for one committed repair decision and its revision-bound verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RepairCycleRecord {
    attempt: u32,
    revision: WorkspaceRevision,
    command_scope_digest: String,
    verification_passed: bool,
}

impl AgentLoopState {
    fn new(
        messages: Vec<ModelMessage>,
        model_turn_limit: u32,
        context_trace: Option<AgentContextTrace>,
    ) -> Self {
        Self {
            messages,
            provider_reasoning_history: Vec::new(),
            tool_result_occurrences: Vec::new(),
            pending_approvals: Vec::new(),
            used_approval_grants: BTreeSet::new(),
            prior_approval_count: 0,
            completion: CompletionTracker::default(),
            repair_state: None,
            repair_attempts: 0,
            repair_cycles: Vec::new(),
            last_completion_error: None,
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
        let tool_results = self
            .tool_result_occurrences
            .into_iter()
            .map(ToolResultOccurrence::into_result)
            .collect::<Vec<_>>();
        let verification = self.completion.summary();
        AgentLoopResult {
            status,
            completed,
            final_answer,
            model_turns,
            tool_calls: tool_results.len() as u32,
            approval_count,
            pending_approvals: self.pending_approvals,
            tool_results,
            verification,
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
        if let Some(metadata) = &error.provider_attempt_metadata {
            self.record_provider_attempts(metadata, model_turn_ordinal, None);
        }
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
            self.record_provider_attempts(metadata, model_turn_ordinal, Some(parent_occurrence_id));
        }
        if let Some(metadata) = &response.provider_capability_metadata {
            self.record_provider_capability_metadata(
                metadata,
                model_turn_ordinal,
                Some(parent_occurrence_id),
            );
        }
    }

    /// Accumulate one provider-owned attempt aggregate at its AgentLoop ownership boundary.
    ///
    /// Negotiation failures have no PromptAssembly parent, while model responses bind attempts
    /// to the emitted prompt occurrence. The same metadata is consumed exactly once by the
    /// caller; trace occurrences remain the independent occurrence fact source.
    fn record_provider_attempts(
        &mut self,
        metadata: &ProviderAttemptMetadata,
        model_turn_ordinal: u32,
        parent_occurrence_id: Option<&str>,
    ) {
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
        let parent_occurrence_id = parent_occurrence_id.map(str::to_string);
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
                    occurrence.parent_occurrence_id = parent_occurrence_id.clone();
                    occurrence
                }),
        );
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
        let checkpoint = ApprovalCheckpoint {
            pending_tool_call: pending_tool_call.clone(),
            state: self.checkpoint_state(input, model_turns, APPROVAL_CHECKPOINT_VERSION, true),
        };
        checkpoint.validate_serialized()?;
        Ok(checkpoint)
    }

    fn turn_checkpoint(
        &self,
        input: &AgentLoopInput,
        model_turns: u32,
        pending_tool_calls: Vec<PendingToolCall>,
    ) -> Result<TurnCheckpoint, String> {
        let checkpoint = TurnCheckpoint {
            state: self.checkpoint_state(input, model_turns, TURN_CHECKPOINT_VERSION, false),
            pending_tool_calls,
        };
        checkpoint.encode().map(|_| checkpoint)
    }

    fn checkpoint_state(
        &self,
        input: &AgentLoopInput,
        model_turns: u32,
        checkpoint_version: u32,
        approval: bool,
    ) -> checkpoint::CheckpointState {
        // Runtime provider occurrences are delivery-scoped; checkpoint only carries the
        // aggregate counters so a decoded resume emits new observations exactly once.
        let mut provider_attempts = self.provider_attempts.clone();
        provider_attempts.occurrences.clear();
        checkpoint::CheckpointState {
            checkpoint_version,
            thread_id: input.thread_id.clone(),
            turn_id: input.turn_id.clone(),
            project_instructions_digest: input.project_instructions_digest.clone(),
            messages: self.messages.clone(),
            provider_reasoning_history: self.provider_reasoning_history.clone(),
            tool_result_occurrences: self.tool_result_occurrences.clone(),
            used_approval_grants: self.used_approval_grants.iter().cloned().collect(),
            approval_count: if approval {
                self.approval_count().saturating_add(1)
            } else {
                0
            },
            model_turns,
            resume_attempt: input.resume_attempt,
            completion: self.completion.clone(),
            repair_state: self.repair_state.clone(),
            repair_attempts: self.repair_attempts,
            repair_cycles: self.repair_cycles.clone(),
            last_completion_error: self.last_completion_error.clone(),
            recovery_metrics: self.recovery_metrics.clone(),
            model_usage: self.model_usage.clone(),
            provider_attempts,
            context_trace: self.context_trace.clone(),
            seen_tool_call_fingerprints: self.seen_tool_call_fingerprints.iter().cloned().collect(),
            last_repair_failure: self.last_repair_failure.clone(),
        }
    }

    fn completion_ready(&self) -> bool {
        // The runtime completion gate is the sole authority for a terminal response.
        self.completion.allows_final()
    }

    fn repair_feedback_with_failure(&self, failure: Option<&ToolResult>) -> String {
        let Some(state) = self.repair_state.as_ref() else {
            return REPAIR_STATE_INSTRUCTIONS.to_string();
        };
        let reason = state.reason;
        let context = self.build_repair_context(reason, failure);
        let context = serde_json::to_string(&context).unwrap_or_else(|_| "{}".to_string());
        let context = if context.chars().count() <= MAX_REPAIR_CONTEXT_SERIALIZED_CHARS {
            context
        } else {
            // Preserve the required shape without admitting an unbounded model message if a
            // future producer weakens one of the field-level limits.
            json!({
                "failed_requirement": "bounded repair context exceeded its safe limit",
                "evidence": "bounded repair evidence unavailable",
                "workspace_revision": self.completion.workspace_revision.map(|revision| revision.value()),
                "previous_action": "bounded repair action unavailable",
                "previous_result": "bounded repair result unavailable",
            })
            .to_string()
        };
        format!("{REPAIR_STATE_INSTRUCTIONS} repair_context={context}")
    }

    fn schedule_repair(
        &mut self,
        reason: AgentRepairReason,
        signature: impl Into<String>,
        failed_tool_name: Option<&str>,
    ) -> Result<RepairState, RepairState> {
        let signature = signature.into();
        // A prospective repair decision does not own the already-failing revision. Only a later
        // mutation can bind the decision to a revision and make its verification cycle consume
        // one repair attempt.
        let required_revision = None;
        if let Some(active) = self.repair_state.as_mut() {
            if self.repair_attempts >= MAX_REPAIR_ATTEMPTS && active.required_revision.is_none() {
                active.reason = reason;
                active.attempt = MAX_REPAIR_ATTEMPTS.saturating_add(1);
                active.max_attempts = MAX_REPAIR_ATTEMPTS;
                active.required_revision = required_revision;
                active.signature = signature;
                active.failed_tool_name = failed_tool_name.map(str::to_string);
                return Err(active.clone());
            }
            // A repeated failure, read, or approval resume does not replace the
            // repair decision or create another prospective attempt. A real verification failure
            // on the bound mutation may upgrade the causal reason without consuming an attempt.
            let terminal_cycle_revision = active
                .required_revision
                .filter(|revision| Some(*revision) == self.completion.workspace_revision);
            active.required_revision = terminal_cycle_revision;
            if reason == AgentRepairReason::VerificationFailed && terminal_cycle_revision.is_some()
            {
                active.reason = reason;
                active.signature = signature;
                active.failed_tool_name = None;
            } else if active.reason == AgentRepairReason::ToolFailure
                && reason == AgentRepairReason::ToolFailure
                && active.failed_tool_name.as_deref() == failed_tool_name
            {
                // A generic completion rejection or a different failed tool cannot replace the
                // concrete call that must be corrected. Otherwise a later successful call to the
                // original tool cannot close its repair decision.
                active.signature = signature;
                active.failed_tool_name = failed_tool_name.map(str::to_string);
            }
            return Ok(active.clone());
        }

        let attempt = self.repair_attempts.saturating_add(1);
        let state = RepairState {
            reason,
            attempt,
            max_attempts: MAX_REPAIR_ATTEMPTS,
            required_revision,
            signature,
            failed_tool_name: failed_tool_name.map(str::to_string),
        };
        let exhausted = attempt > MAX_REPAIR_ATTEMPTS;
        self.repair_state = Some(state.clone());
        if exhausted { Err(state) } else { Ok(state) }
    }

    fn build_repair_context(
        &self,
        reason: AgentRepairReason,
        failure: Option<&ToolResult>,
    ) -> Value {
        let summary = self.completion.summary();
        let latest_result = failure.or_else(|| {
            self.tool_result_occurrences
                .last()
                .map(ToolResultOccurrence::result)
        });
        let requires_strategy_change = self.repair_state.as_ref().is_some_and(|active| {
            active.required_revision.is_none()
                && matches!(reason, AgentRepairReason::RevisionConflict)
        });
        let active_tool_failure = (reason == AgentRepairReason::ToolFailure)
            .then(|| {
                let failed_tool_name = self
                    .repair_state
                    .as_ref()
                    .and_then(|active| active.failed_tool_name.as_deref());
                self.tool_result_occurrences
                    .iter()
                    .rev()
                    .map(ToolResultOccurrence::result)
                    .find(|result| {
                        !result.ok
                            && is_repairable_tool_result(result)
                            && failed_tool_name.is_none_or(|name| result.tool_name == name)
                    })
            })
            .flatten();
        // A later diagnostic command is useful transcript evidence, but it must not replace the
        // failure that owns the active repair decision.
        let evidence_result = failure
            .filter(|result| !result.ok)
            .or(active_tool_failure)
            .or_else(|| {
                (!requires_strategy_change)
                    .then_some(latest_result)
                    .flatten()
            });
        let failed_requirement = requires_strategy_change
            .then(|| self.last_completion_error.clone())
            .flatten()
            .or_else(|| summary.unresolved_failures.first().cloned())
            .or_else(|| self.last_completion_error.clone())
            .unwrap_or_else(|| repair_reason_text(reason).to_string());
        // Mutation scope and digests remain in the trusted tool history. They are intentionally not
        // copied into a latest-only repair state or compared against a later model response.
        let previous_result = requires_strategy_change
            .then(|| {
                self.last_completion_error
                    .as_deref()
                    .map(|error| json!(bounded_repair_text(error)))
            })
            .flatten()
            .or_else(|| latest_result.map(|result| json!(safe_tool_result_evidence(result))))
            .or_else(|| {
                self.tool_result_occurrences
                    .last()
                    .map(|occurrence| json!(safe_tool_result_evidence(occurrence.result())))
            })
            .or_else(|| {
                self.last_completion_error
                    .as_deref()
                    .map(|error| json!(bounded_repair_text(error)))
            })
            .unwrap_or_else(|| json!("repair decision pending execution"));
        // Keep the repair context tied to the latest typed result without duplicating mutation
        // scope or digest state outside the complete tool history.
        let previous_action = self
            .tool_result_occurrences
            .last()
            .map(|occurrence| json!(safe_repair_tool_name(occurrence.result())))
            .or_else(|| failure.map(|result| json!(safe_repair_tool_name(result))))
            .unwrap_or_else(|| json!(repair_reason_text(reason)));
        json!({
            "failed_requirement": bounded_repair_text(&failed_requirement),
            "evidence": evidence_result
                .map(safe_tool_result_evidence)
                .unwrap_or_else(|| {
                    previous_result
                        .as_str()
                        .map(bounded_repair_text)
                        .unwrap_or_else(|| bounded_repair_text(&previous_result.to_string()))
                }),
            "workspace_revision": self.completion.workspace_revision.map(|revision| revision.value()),
            "previous_action": previous_action,
            "previous_result": previous_result,
            "repair_strategy_change_required": requires_strategy_change,
        })
    }

    fn note_repair_mutation(&mut self, revision: WorkspaceRevision) {
        let Some(active) = self.repair_state.as_mut() else {
            return;
        };
        active.attempt = self.repair_attempts.saturating_add(1);
        active.required_revision = Some(revision);
    }

    /// Commit exactly one repair attempt for a new mutation revision followed by its terminal
    /// command observation.  A failed command remains visible as a consumed cycle and opens the
    /// next prospective attempt; the third failed cycle terminates before another mutation can
    /// be requested.
    fn consume_repair_cycle_if_complete(
        &mut self,
        tool_name: &str,
        result: &ToolResult,
    ) -> Option<RepairCycleCommit> {
        if tool_name != TOOL_COMMAND || result.tool_name != TOOL_COMMAND {
            return None;
        }
        if !result.ok && !is_repairable_tool_result(result) {
            return None;
        }
        let observation = result.workspace_observation()?;
        if observation.mutation() != singularity_tools::WorkspaceMutation::Unchanged {
            return None;
        }
        let revision = observation.revision()?;
        if self.completion.workspace_revision != Some(revision) {
            return None;
        }
        let command_scope_digest = tool_result_command_scope_digest(result)?;
        // A successful command only closes the cycle after every required command for this
        // revision has been observed.  A matching failure closes the failed cycle immediately.
        if result.ok && !self.completion.verification_satisfied() {
            return None;
        }
        let active = self.repair_state.as_mut()?;
        if active.required_revision != Some(revision) {
            return None;
        }
        self.repair_attempts = self.repair_attempts.saturating_add(1);
        self.recovery_metrics.repair_attempt_count = self.repair_attempts;
        active.attempt = self.repair_attempts;
        let committed_attempt = self.repair_attempts;
        self.repair_cycles.push(RepairCycleRecord {
            attempt: committed_attempt,
            revision,
            command_scope_digest: command_scope_digest.to_string(),
            verification_passed: result.ok,
        });
        if result.ok {
            self.repair_state = None;
            self.last_repair_failure = None;
            return Some(RepairCycleCommit {
                attempt: committed_attempt,
                exhausted: false,
            });
        }
        if committed_attempt >= MAX_REPAIR_ATTEMPTS {
            // Keep an internal terminal marker so the current tool batch fails immediately.  The
            // event projection below uses the committed attempt (3), never this sentinel (4).
            active.attempt = MAX_REPAIR_ATTEMPTS.saturating_add(1);
            active.required_revision = None;
            Some(RepairCycleCommit {
                attempt: committed_attempt,
                exhausted: true,
            })
        } else {
            // The failed verification is committed, but the next prospective attempt remains
            // uncommitted until a new mutation revision is observed.
            active.required_revision = None;
            active.attempt = self.repair_attempts.saturating_add(1);
            Some(RepairCycleCommit {
                attempt: committed_attempt,
                exhausted: false,
            })
        }
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
            let tool_failure_resolved = self.repair_state.as_ref().is_some_and(|state| {
                if state.reason != AgentRepairReason::ToolFailure
                    || state.required_revision.is_some()
                {
                    return false;
                }
                match state.failed_tool_name.as_deref() {
                    Some(failed_tool_name) => failed_tool_name == tool_result.tool_name,
                    None => true,
                }
            });
            if tool_failure_resolved {
                // Mutation and verification failures require the same tool to succeed. A
                // read-only or visibility failure may be superseded by any successful action
                // because its typed result remains in the model-visible transcript.
                self.repair_state = None;
            }
            self.last_repair_failure = None;
            return None;
        }
        if !is_repairable_tool_result(tool_result) {
            self.last_repair_failure = None;
            return None;
        }
        let error_code = tool_result
            .error_code
            .as_deref()
            .unwrap_or("tool_execution_failed");
        // The public command error is intentionally coarse; the validated audit code retains the
        // causal distinction needed to avoid treating different input repairs as one failure.
        let repair_error_code = tool_result
            .audit_metadata()
            .and_then(Value::as_object)
            .and_then(|audit| audit.get("argument_validation_code"))
            .and_then(Value::as_str)
            .unwrap_or(error_code);
        let signature = repair_failure_signature(tool_call_fingerprint, repair_error_code);
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
        let repair_signature = self
            .last_repair_failure
            .as_ref()
            .map(|failure| failure.signature.clone())
            .unwrap_or_default();
        let repair_reason = AgentRepairReason::ToolFailure;
        let failed_tool_name = matches!(tool_result.tool_name.as_str(), TOOL_PATCH | TOOL_COMMAND)
            .then_some(tool_result.tool_name.as_str());
        if let Err(exhausted) =
            self.schedule_repair(repair_reason, repair_signature, failed_tool_name)
        {
            self.repair_state = Some(RepairState {
                reason: exhausted.reason,
                attempt: exhausted.attempt,
                max_attempts: exhausted.max_attempts,
                required_revision: exhausted.required_revision,
                signature: String::new(),
                failed_tool_name: failed_tool_name.map(str::to_string),
            });
            return Some("repair budget exhausted; refusing another repair attempt".to_string());
        }
        (consecutive_count >= 2).then(|| REPEATED_FAILURE_RECOVERY_INSTRUCTIONS.to_string())
    }

    fn append_visible_tool_result(&mut self, tool_result: ToolResult) {
        self.messages.push(tool_result_message(&tool_result));
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

fn is_provider_history_validation_rejection(result: &ToolResult) -> bool {
    if !matches!(
        result.error_code.as_deref(),
        Some("tool_not_visible" | "invalid_tool_arguments")
    ) {
        return false;
    }
    let Some(audit) = result.audit_metadata().and_then(Value::as_object) else {
        return false;
    };
    audit.get("argument_validation").and_then(Value::as_str) == Some("failed")
        && audit.get("executor_started").and_then(Value::as_bool) == Some(false)
}

/// 描述导致多调用批次整批拒绝的首个安全边界，不携带原始参数。
struct BatchRejectionTrigger {
    tool_name: String,
    error_code: String,
    execution_mode: Option<ToolExecutionMode>,
    category: &'static str,
    reason: &'static str,
}

fn batch_rejection_trigger(prepared: &[PreparedToolCall]) -> Option<BatchRejectionTrigger> {
    prepared
        .iter()
        .find_map(|call| {
            call.rejection.as_ref().map(|result| {
                BatchRejectionTrigger {
                    tool_name: call.call.tool_name.clone(),
                    error_code: result
                        .error_code
                        .clone()
                        .unwrap_or_else(|| "tool_preflight_rejected".to_string()),
                    execution_mode: call.bound.as_ref().map(|bound| bound.execution_mode),
                    category: "preflight_failure",
                    reason: "the batch contains a call rejected during preflight validation",
                }
            })
        })
        .or_else(|| {
            prepared.iter().find_map(|call| {
                matches!(call.decision, Some(ToolBrokerDecision::Ask { .. })).then(|| {
                    BatchRejectionTrigger {
                        tool_name: call.call.tool_name.clone(),
                        error_code: "approval_required".to_string(),
                        execution_mode: call.bound.as_ref().map(|bound| bound.execution_mode),
                        category: "approval_sensitive",
                        reason: "the batch contains an approval-sensitive call that requires approval",
                    }
                })
            })
        })
        .or_else(|| {
            prepared.iter().find_map(|call| {
                (call.bound.as_ref().map(|bound| bound.execution_mode)
                    == Some(ToolExecutionMode::Exclusive))
                .then(|| BatchRejectionTrigger {
                    tool_name: call.call.tool_name.clone(),
                    error_code: "exclusive_tool_requires_single_call".to_string(),
                    execution_mode: Some(ToolExecutionMode::Exclusive),
                    category: "exclusive",
                    reason: "the batch contains an exclusive call that must be submitted alone",
                })
            })
        })
}

fn batch_rejection_contract_result(
    prepared: &PreparedToolCall,
    trigger: &BatchRejectionTrigger,
    mut result: ToolResult,
) -> ToolResult {
    const REQUIRED_NEXT_ACTION: &str = "Correct any preflight failure first. Submit each exclusive, mutation, command, or approval-sensitive call alone, then wait for its result before submitting dependent calls. Independent read-only calls may be submitted together.";

    let audit = result.audit_metadata().cloned();
    let mut content = match result.content.take() {
        Some(Value::Object(content)) => content,
        Some(content) => serde_json::Map::from_iter([("detail".to_string(), content)]),
        None => result
            .preview
            .take()
            .map_or_else(serde_json::Map::new, |preview| {
                serde_json::Map::from_iter([("detail".to_string(), json!(preview))])
            }),
    };
    content.insert("batch_executed".to_string(), json!(false));
    content.insert("call_executed".to_string(), json!(false));
    content.insert(
        "call_preflight_status".to_string(),
        json!(if prepared.rejection.is_some() {
            "rejected"
        } else {
            "passed"
        }),
    );
    if let Some(execution_mode) = prepared.bound.as_ref().map(|bound| bound.execution_mode) {
        content.insert("execution_mode".to_string(), json!(execution_mode));
    } else {
        content.insert("safety_category".to_string(), json!("preflight_failure"));
    }
    content.insert("rejection_reason".to_string(), json!(trigger.reason));
    content.insert("trigger_tool_name".to_string(), json!(trigger.tool_name));
    content.insert("trigger_error_code".to_string(), json!(trigger.error_code));
    if let Some(execution_mode) = trigger.execution_mode {
        content.insert("trigger_execution_mode".to_string(), json!(execution_mode));
    }
    content.insert("trigger_category".to_string(), json!(trigger.category));
    content.insert(
        "required_next_action".to_string(),
        json!(REQUIRED_NEXT_ACTION),
    );

    let output = ToolOutput::failure_with_kind(
        result
            .failure_kind
            .clone()
            .unwrap_or(ToolFailureKind::Capability),
        result
            .error_code
            .clone()
            .unwrap_or_else(|| "tool_batch_rejected".to_string()),
        Value::Object(content),
    );
    let result_call = if is_provider_history_validation_rejection(&result) {
        provider_history_rejected_tool_call(&prepared.call)
    } else {
        prepared.call.clone()
    };
    let mut enriched = ToolResult::from_result(&tool_call_request(&result_call), &output);
    if let Some(audit) = audit {
        enriched = enriched.with_audit(audit);
    }
    enriched
}

enum ToolBatchControl {
    Continue,
    Blocked,
    Failed(String),
    Cancelled,
}

/// Result of committing a revision-bound repair cycle.  The committed attempt is kept separate
/// from the active repair state's prospective attempt so exhaustion never leaks an attempt four event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RepairCycleCommit {
    attempt: u32,
    exhausted: bool,
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

    /// Run while notifying the owner at durable input/tool-result boundaries.
    pub fn run_with_events_and_checkpoints(
        &self,
        input: &AgentLoopInput,
        on_event: &mut AgentLoopEventCallback<'_>,
        on_checkpoint: &mut AgentLoopCheckpointCallback<'_>,
    ) -> AgentLoopResult {
        self.run_internal_with_checkpoints(input, Some(on_event), Some(on_checkpoint))
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

    /// Build the first durable boundary for a turn before any model request or tool side effect.
    /// The snapshot contains only the complete, public input context and no partial provider
    /// response; callers persist it before entering the execution owner.
    pub fn initial_turn_checkpoint(
        &self,
        input: &AgentLoopInput,
    ) -> Result<TurnCheckpoint, String> {
        validate_replay_preflight(input)?;
        let public_budget = ContextBudget::for_public_assembly(u32::MAX);
        let state = if let Some(seed) = input.historical_checkpoint.as_ref() {
            let current_user_text = current_user_text_from_input(input);
            let messages = prepare_seed_messages(seed, input, 1, &current_user_text);
            let mut state = AgentLoopState::new(messages, input.max_turns.max(1), None);
            state.provider_reasoning_history = seed.provider_reasoning_history.clone();
            state.tool_result_occurrences = seed.tool_result_occurrences.clone();
            state
        } else {
            let context = assemble_context_items_with_budget(&input.input, &public_budget);
            if current_turn_excluded(input, &context) {
                return Err(CURRENT_TURN_CONTEXT_OVERFLOW_ERROR.to_string());
            }
            let messages = model_messages_from_input(input, &context, 1);
            let mut state = AgentLoopState::new(messages, input.max_turns.max(1), None);
            state.provider_reasoning_history = input.provider_reasoning_history.clone();
            state.context_trace = Some(AgentContextTrace::from(&context));
            state
        };
        state
            .turn_checkpoint(input, 0, Vec::new())
            .map_err(|error| format!("initial turn checkpoint invalid: {error}"))
    }

    /// Resume a non-approval turn from a validated durable boundary. No tool call is replayed;
    /// the next provider request is built from the checkpoint's exact message/state snapshot.
    pub fn resume_turn(
        &self,
        input: &AgentLoopInput,
        checkpoint: &TurnCheckpoint,
    ) -> AgentLoopResult {
        self.resume_turn_internal(input, checkpoint, None, None)
    }

    /// Resume a non-approval turn and project the same observable events as a fresh run.
    pub fn resume_turn_with_events(
        &self,
        input: &AgentLoopInput,
        checkpoint: &TurnCheckpoint,
        on_event: &mut AgentLoopEventCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_turn_internal(input, checkpoint, Some(on_event), None)
    }

    /// Resume a non-approval turn while notifying the owner at durable tool boundaries.
    pub fn resume_turn_with_events_and_checkpoints(
        &self,
        input: &AgentLoopInput,
        checkpoint: &TurnCheckpoint,
        on_event: &mut AgentLoopEventCallback<'_>,
        on_checkpoint: &mut AgentLoopCheckpointCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_turn_internal(input, checkpoint, Some(on_event), Some(on_checkpoint))
    }

    fn resume_turn_internal(
        &self,
        input: &AgentLoopInput,
        checkpoint: &TurnCheckpoint,
        mut on_event: Option<&mut AgentLoopEventCallback<'_>>,
        mut on_checkpoint: Option<&mut AgentLoopCheckpointCallback<'_>>,
    ) -> AgentLoopResult {
        let (state, model_turn_offset) = match restore_turn_checkpoint(input, checkpoint) {
            Ok(restored) => restored,
            Err(error) => return failed_result(error),
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
                    "turn checkpoint workspace revision binding failed: {error}"
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
        if !checkpoint.pending_tool_calls().is_empty() {
            let mut occurrences = Vec::with_capacity(checkpoint.pending_tool_calls().len());
            for (ordinal, pending) in checkpoint.pending_tool_calls().iter().enumerate() {
                let call = match pending.to_model_tool_call() {
                    Ok(call) => call,
                    Err(error) => {
                        return state.finish(
                            AgentStatus::Failed,
                            false,
                            None,
                            model_turn_offset,
                            Some(format!(
                                "turn checkpoint pending tool call is invalid: {error}"
                            )),
                        );
                    }
                };
                occurrences.push(ModelToolOccurrence {
                    fingerprint: tool_call_fingerprint(&call),
                    invalid_was_observed: false,
                    context: tool_occurrence_context(
                        input,
                        &call,
                        model_turn_offset,
                        u32::try_from(ordinal).unwrap_or(u32::MAX),
                    ),
                    call,
                });
            }
            match self.process_tool_calls(
                input,
                &occurrences,
                &mut state,
                model_turn_offset,
                None,
                &mut on_event,
                &mut on_checkpoint,
            ) {
                ToolBatchControl::Continue => {}
                ToolBatchControl::Blocked => {
                    return state.finish(
                        AgentStatus::Blocked,
                        false,
                        None,
                        model_turn_offset,
                        None,
                    );
                }
                ToolBatchControl::Cancelled => {
                    return state.finish(
                        AgentStatus::Cancelled,
                        false,
                        None,
                        model_turn_offset,
                        None,
                    );
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
            }
        }
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            model_turn_offset,
            on_event,
            on_checkpoint,
        )
    }

    fn run_internal(
        &self,
        input: &AgentLoopInput,
        on_event: Option<&mut AgentLoopEventCallback<'_>>,
    ) -> AgentLoopResult {
        self.run_internal_with_checkpoints(input, on_event, None)
    }

    fn run_internal_with_checkpoints(
        &self,
        input: &AgentLoopInput,
        mut on_event: Option<&mut AgentLoopEventCallback<'_>>,
        mut on_checkpoint: Option<&mut AgentLoopCheckpointCallback<'_>>,
    ) -> AgentLoopResult {
        let state = AgentLoopState::new(Vec::new(), input.max_turns.max(1), None);
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, 0, None);
        }
        // 网络前拒绝不兼容的私有 replay，先于 capability probe 与 Initial checkpoint。
        if let Err(error) = validate_replay_preflight(input) {
            return AgentLoopState::new(Vec::new(), input.max_turns.max(1), None).finish(
                AgentStatus::Failed,
                false,
                None,
                0,
                Some(error),
            );
        }
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
        if let Some(seed) = input.historical_checkpoint.as_ref() {
            // 跨轮 seed：完整历史消息 + 已持久化 occurrence 直接作为新 Turn 前缀。
            let current_user_text = current_user_text_from_input(input);
            state.messages = prepare_seed_messages(seed, input, max_tool_calls, &current_user_text);
            state.provider_reasoning_history = seed.provider_reasoning_history.clone();
            state.tool_result_occurrences = seed.tool_result_occurrences.clone();
        } else {
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
            state.provider_reasoning_history = input.provider_reasoning_history.clone();
            state.context_trace = Some(AgentContextTrace::from(&context));
        }
        if let Some(callback) = on_checkpoint.as_deref_mut() {
            let checkpoint = match state.turn_checkpoint(input, 0, Vec::new()) {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        0,
                        Some(format!("turn checkpoint failed: {error}")),
                    );
                }
            };
            if callback(TurnCheckpointEvent {
                phase: TurnCheckpointPhase::Initial,
                checkpoint,
            })
            .is_err()
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    0,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
        }
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            0,
            on_event,
            on_checkpoint,
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
        on_checkpoint: Option<&mut AgentLoopCheckpointCallback<'_>>,
    ) -> AgentLoopResult {
        let max_turns = input.max_turns.max(1);
        if model_turn_offset > max_turns
            || (model_turn_offset == max_turns && !state.completion_ready())
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
        let mut on_event = on_event;
        let mut on_checkpoint = on_checkpoint;
        let mut actual_model_turns = model_turn_offset;
        // The inclusive endpoint is reserved for one terminal response only when the runtime
        // completion gate became ready on the last ordinary work turn.
        for turn_index in model_turn_offset..=max_turns {
            if state
                .repair_state
                .as_ref()
                .is_some_and(|state| state.attempt > state.max_attempts)
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    actual_model_turns,
                    Some("repair budget exhausted".to_string()),
                );
            }
            let finalization_only = turn_index == max_turns && state.completion_ready();
            if turn_index == max_turns && !finalization_only {
                break;
            }
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index, None);
            }
            if !matches!(
                self.emit_checkpoint_event(
                    input,
                    &state,
                    TurnCheckpointPhase::BeforeModelRequest { finalization_only },
                    turn_index,
                    &mut on_checkpoint,
                ),
                ToolBatchControl::Continue
            ) {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(EVENT_SINK_FAILURE_ERROR.to_string()),
                );
            }
            if finalization_only {
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
            let mut compacted = false;
            if !model_request_fits_context(
                &tool_view.tools,
                &state.messages,
                &state.tool_result_occurrences,
                &state.provider_reasoning_history,
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
                        &state.provider_reasoning_history,
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
                state
                    .provider_reasoning_history
                    .retain(|replay| replay.is_bound_to_messages(&state.messages));
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
            // The projection is request-scoped. Tool-result and later checkpoints must derive the
            // next action from trusted state instead of persisting a now-stale command instruction.
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
                    &state.provider_reasoning_history,
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
                &state.provider_reasoning_history,
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
            let provider_events = RefCell::new(ProviderEventBridge::new(
                prompt_identity.clone(),
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
                    if provider_events.borrow().streamed_text.is_empty() {
                        let stream_error = error;
                        let completion = {
                            let mut on_attempt =
                                |event| provider_events.borrow_mut().on_attempt(event);
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
                    } else {
                        provider_error_model_response(&request, error)
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
            if response
                .provider_reasoning_history
                .iter()
                .any(|replay| !replay.is_valid())
            {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some("provider reasoning replay is invalid".to_string()),
                );
            }
            let buffered_text_deltas = provider_events.into_buffered_text_deltas();
            state.observe_model_response(&response, turn_index, &prompt_identity.occurrence_id);
            if !response.provider_reasoning_history.is_empty() {
                for replay in &response.provider_reasoning_history {
                    if !state.provider_reasoning_history.contains(replay) {
                        state.provider_reasoning_history.push(replay.clone());
                    }
                }
            }
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
            let recoverable_tool_validation = !finalization_only
                && recoverable_tool_response_validation(&response, &validation.errors);
            if !validation.valid && !recoverable_tool_validation {
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
                if state.completion_ready() {
                    if !finalization_only
                        && emit_verification_occurrence(
                            &mut on_event,
                            input,
                            turn_index,
                            state.recovery_metrics.completion_rejection_count,
                            "verification_gate",
                            VerificationStatus::GatePassed,
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
                    for delta in buffered_text_deltas {
                        if emit_event(&mut on_event, AgentLoopEvent::FinalTextDelta { delta })
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
                    state.repair_state = None;
                    state.last_repair_failure = None;
                    state.last_completion_error = None;
                    state.messages.push(ModelMessage::text(
                        ModelRole::Assistant,
                        final_answer.clone(),
                    ));
                    if !matches!(
                        self.emit_checkpoint_event(
                            input,
                            &state,
                            TurnCheckpointPhase::ModelResponseCommitted,
                            actual_model_turns,
                            &mut on_checkpoint,
                        ),
                        ToolBatchControl::Continue
                    ) {
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
                let repair_requested = if state.completion.allows_final() {
                    false
                } else {
                    match state.schedule_repair(
                        AgentRepairReason::VerificationFailed,
                        COMPLETION_REPAIR_SIGNATURE,
                        None,
                    ) {
                        Ok(_state) => true,
                        Err(exhausted) => {
                            state.repair_state = Some(RepairState {
                                reason: exhausted.reason,
                                attempt: exhausted.attempt,
                                max_attempts: exhausted.max_attempts,
                                required_revision: exhausted.required_revision,
                                signature: COMPLETION_REPAIR_SIGNATURE.to_string(),
                                failed_tool_name: None,
                            });
                            false
                        }
                    }
                };
                state.recovery_metrics.completion_rejection_count = state
                    .recovery_metrics
                    .completion_rejection_count
                    .saturating_add(1);
                let rejection_count = state.recovery_metrics.completion_rejection_count;
                let verification = state.completion.summary();
                let verification_statuses = if repair_requested {
                    [
                        VerificationStatus::GateRejected,
                        VerificationStatus::RepairRequested,
                    ]
                    .into_iter()
                    .collect::<Vec<_>>()
                } else {
                    vec![VerificationStatus::GateRejected]
                };
                for status in verification_statuses {
                    let kind = match status {
                        VerificationStatus::GateRejected => "verification_gate_rejected",
                        VerificationStatus::RepairRequested => "verification_repair_requested",
                        _ => "verification_gate_rejected",
                    };
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
                state.last_completion_error = Some(state.completion.rejection_reason());
                state.messages.push(
                    response
                        .assistant_message
                        .unwrap_or_else(|| ModelMessage::text(ModelRole::Assistant, final_answer)),
                );
                state.messages.push(ModelMessage::text(
                    ModelRole::Developer,
                    state.completion.feedback(),
                ));
                continue;
            }
            let execution_tool_calls =
                resolve_model_tool_calls(&response.tool_calls, &provider_tool_names);
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
            match self.process_tool_calls(
                input,
                &tool_occurrences,
                &mut state,
                actual_model_turns,
                response.assistant_message.as_ref(),
                &mut on_event,
                &mut on_checkpoint,
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
        self.resume_pending_approval_internal(input, pending, None, None)
    }

    /// 恢复 approval，并在真实边界向调用方投影有序 typed 事件。
    pub fn resume_pending_approval_with_events(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        on_event: &mut AgentLoopEventCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_pending_approval_internal(input, pending, Some(on_event), None)
    }

    /// Resume an approved tool call while notifying the owner at durable tool-result boundaries.
    pub fn resume_pending_approval_with_events_and_checkpoints(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        on_event: &mut AgentLoopEventCallback<'_>,
        on_checkpoint: &mut AgentLoopCheckpointCallback<'_>,
    ) -> AgentLoopResult {
        self.resume_pending_approval_internal(input, pending, Some(on_event), Some(on_checkpoint))
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
            None,
        )
    }

    fn resume_pending_approval_internal(
        &self,
        input: &AgentLoopInput,
        pending: &PendingApprovalOccurrence,
        mut on_event: Option<&mut AgentLoopEventCallback<'_>>,
        mut on_checkpoint: Option<&mut AgentLoopCheckpointCallback<'_>>,
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
        if !matches!(
            self.emit_checkpoint_event(
                input,
                &state,
                TurnCheckpointPhase::ToolCallsReady {
                    pending_tool_calls: vec![pending.pending_tool_call().clone()],
                },
                model_turn_offset,
                &mut on_checkpoint,
            ),
            ToolBatchControl::Continue
        ) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some("tool-call checkpoint persistence failed".to_string()),
            );
        }
        let tool_call_id = occurrence.call.tool_call_id.clone();
        let runtime = self.execute_tool(
            &prepared,
            observed_decision.decision,
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
        if !matches!(
            self.emit_checkpoint_event(
                input,
                &state,
                TurnCheckpointPhase::ToolResultsCommitted {
                    tool_call_ids: vec![tool_call_id],
                },
                model_turn_offset,
                &mut on_checkpoint,
            ),
            ToolBatchControl::Continue
        ) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                model_turn_offset,
                Some("tool-result checkpoint persistence failed".to_string()),
            );
        }
        self.continue_run(
            input,
            &budget,
            &capabilities,
            max_tool_calls,
            state,
            model_turn_offset,
            on_event,
            on_checkpoint,
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
    #[allow(clippy::too_many_arguments)]
    fn process_tool_calls(
        &self,
        input: &AgentLoopInput,
        occurrences: &[ModelToolOccurrence],
        state: &mut AgentLoopState,
        next_model_turn: u32,
        original_assistant_message: Option<&ModelMessage>,
        on_event: &mut Option<&mut AgentLoopEventCallback<'_>>,
        on_checkpoint: &mut Option<&mut AgentLoopCheckpointCallback<'_>>,
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

        if let Some(original_assistant_message) = original_assistant_message {
            let rejected_calls = prepared
                .iter()
                .map(|call| {
                    call.rejection
                        .as_ref()
                        .is_some_and(is_provider_history_validation_rejection)
                })
                .collect::<Vec<_>>();
            state.messages.push(provider_history_assistant_message(
                Some(original_assistant_message),
                &occurrences
                    .iter()
                    .map(|occurrence| occurrence.call.clone())
                    .collect::<Vec<_>>(),
                &prepared
                    .iter()
                    .map(|call| call.call.clone())
                    .collect::<Vec<_>>(),
                &rejected_calls,
            ));
        }

        if prepared.len() > 1
            && prepared.iter().any(|call| {
                call.rejection.is_some()
                    || call.bound.as_ref().map(|bound| bound.execution_mode)
                        == Some(ToolExecutionMode::Exclusive)
                    || matches!(call.decision, Some(ToolBrokerDecision::Ask { .. }))
            })
        {
            let trigger = batch_rejection_trigger(&prepared)
                .expect("a rejected batch has a rejection trigger");
            let results = prepared
                .drain(..)
                .map(|call| {
                    let result = self.batch_rejection_result(&call, &trigger);
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
            let pending_tool_calls = prepared
                .iter()
                .map(|call| {
                    PendingToolCall::new(
                        input,
                        &call.call,
                        call.bound.as_ref().expect("prepared read call is bound"),
                    )
                })
                .collect();
            if !matches!(
                self.emit_checkpoint_event(
                    input,
                    state,
                    TurnCheckpointPhase::ToolCallsReady { pending_tool_calls },
                    next_model_turn,
                    on_checkpoint,
                ),
                ToolBatchControl::Continue
            ) {
                return ToolBatchControl::Failed(
                    "tool-call checkpoint persistence failed".to_string(),
                );
            }
            let results = self.execute_parallel_reads(prepared);
            if self.is_cancelled(input) {
                return ToolBatchControl::Cancelled;
            }
            let control =
                self.record_tool_results(input, state, results, occurrences, false, on_event);
            if !matches!(control, ToolBatchControl::Continue) {
                return control;
            }
            let phase = TurnCheckpointPhase::ToolResultsCommitted {
                tool_call_ids: occurrences
                    .iter()
                    .map(|occurrence| occurrence.call.tool_call_id.clone())
                    .collect(),
            };
            if !matches!(
                self.emit_checkpoint_event(input, state, phase, next_model_turn, on_checkpoint),
                ToolBatchControl::Continue
            ) {
                return ToolBatchControl::Failed(
                    "tool-result checkpoint persistence failed".to_string(),
                );
            }
            return ToolBatchControl::Continue;
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
        if !matches!(
            self.emit_checkpoint_event(
                input,
                state,
                TurnCheckpointPhase::ToolCallsReady {
                    pending_tool_calls: vec![PendingToolCall::new(
                        input,
                        &prepared.call,
                        prepared
                            .bound
                            .as_ref()
                            .expect("prepared tool call is bound"),
                    )],
                },
                next_model_turn,
                on_checkpoint,
            ),
            ToolBatchControl::Continue
        ) {
            return ToolBatchControl::Failed("tool-call checkpoint persistence failed".to_string());
        }
        let result = self.execute_tool(
            &prepared,
            decision,
            &occurrences
                .first()
                .expect("single tool occurrence is present")
                .context,
            on_event,
        );
        let control = self.record_tool_results(
            input,
            state,
            vec![(prepared, result)],
            occurrences,
            false,
            on_event,
        );
        if !matches!(control, ToolBatchControl::Continue) {
            return control;
        }
        self.emit_checkpoint_event(
            input,
            state,
            TurnCheckpointPhase::ToolResultsCommitted {
                tool_call_ids: occurrences
                    .first()
                    .map(|occurrence| vec![occurrence.call.tool_call_id.clone()])
                    .unwrap_or_default(),
            },
            next_model_turn,
            on_checkpoint,
        )
    }

    fn emit_checkpoint_event(
        &self,
        input: &AgentLoopInput,
        state: &AgentLoopState,
        phase: TurnCheckpointPhase,
        model_turns: u32,
        on_checkpoint: &mut Option<&mut AgentLoopCheckpointCallback<'_>>,
    ) -> ToolBatchControl {
        let Some(callback) = on_checkpoint.as_deref_mut() else {
            return ToolBatchControl::Continue;
        };
        let pending_tool_calls = match &phase {
            TurnCheckpointPhase::ToolCallsReady { pending_tool_calls } => {
                pending_tool_calls.clone()
            }
            _ => Vec::new(),
        };
        let checkpoint = match state.turn_checkpoint(input, model_turns, pending_tool_calls) {
            Ok(checkpoint) => checkpoint,
            Err(error) => return ToolBatchControl::Failed(error),
        };
        if callback(TurnCheckpointEvent { phase, checkpoint }).is_err() {
            ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string())
        } else {
            ToolBatchControl::Continue
        }
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

    fn batch_rejection_result(
        &self,
        prepared: &PreparedToolCall,
        trigger: &BatchRejectionTrigger,
    ) -> ToolResult {
        let envelope = tool_call_request(&prepared.call);
        let mut result = if let Some(result) = &prepared.rejection {
            result.clone()
        } else {
            match prepared.decision.as_ref() {
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
                    ToolFailureKind::Visibility,
                    "tool_batch_rejected",
                    "the tool batch was rejected before execution",
                ),
            }
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
        batch_rejection_contract_result(prepared, trigger, result)
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
            let verification_occurrence = (prepared.call.tool_name == TOOL_COMMAND).then(|| {
                let summary = state.completion.summary();
                (
                    child_occurrence_identity(&occurrence.context.identity, "verification", 0),
                    summary.required_command_count,
                    occurrence.context.tool_call_ordinal.saturating_add(1),
                )
            });
            if let Some((identity, required_command_count, occurrence_count)) =
                &verification_occurrence
            {
                let summary = state.completion.summary();
                if emit_event(
                    on_event,
                    AgentLoopEvent::Observation(AgentObservation::Verification(
                        VerificationObservation {
                            identity: identity.clone(),
                            lifecycle: occurrence.context.timer.started(),
                            required_command_count: *required_command_count,
                            satisfied_command_count: summary.satisfied_command_count,
                            occurrence_count: *occurrence_count,
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
            let changed = result.workspace_observation().is_some_and(|observation| {
                observation.mutation() == singularity_tools::WorkspaceMutation::Changed
            });
            if changed {
                let Some(revision) = result
                    .workspace_observation()
                    .and_then(|observation| observation.revision())
                else {
                    state
                        .completion
                        .mark_workspace_revision_invalid("mutation_revision_missing");
                    return ToolBatchControl::Failed(
                        "workspace mutation revision is missing".to_string(),
                    );
                };
                match validate_workspace_change_summary(&prepared.call, &result) {
                    Ok(()) => {}
                    Err(error) => {
                        state
                            .completion
                            .mark_workspace_revision_invalid("mutation_diff_summary_invalid");
                        return ToolBatchControl::Failed(error);
                    }
                }
                state.note_repair_mutation(revision);
            }
            let repair_cycle =
                state.consume_repair_cycle_if_complete(&prepared.call.tool_name, &result);
            let _ = repair_cycle;
            if let Some((identity, required_command_count, occurrence_count)) =
                verification_occurrence
            {
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
                            required_command_count,
                            satisfied_command_count: summary.satisfied_command_count,
                            occurrence_count,
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
            // Repair instructions describe current state, so replace their prior projection while
            // retaining the immutable Assistant ToolCall and ToolResult transcript.
            state.messages.retain(|message| {
                message.role != ModelRole::Developer
                    || (message.content != REPEATED_FAILURE_RECOVERY_INSTRUCTIONS
                        && !message.content.starts_with(REPAIR_STATE_INSTRUCTIONS))
            });
            // Append before projecting repair context so the current typed failure is included.
            state.append_visible_tool_result(result.clone());
            let repair_feedback = state
                .repair_state
                .is_some()
                .then(|| state.repair_feedback_with_failure(Some(&result)));
            if let Some(feedback) = recovery_feedback {
                state
                    .messages
                    .push(ModelMessage::text(ModelRole::Developer, feedback));
            }
            if let Some(repair_feedback) = repair_feedback {
                state
                    .messages
                    .push(ModelMessage::text(ModelRole::Developer, repair_feedback));
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
        } else if state
            .repair_state
            .as_ref()
            .is_some_and(|state| state.attempt > state.max_attempts)
        {
            ToolBatchControl::Failed("repair budget exhausted".to_string())
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
    if capabilities.max_output_tokens == 0 {
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
    let input_token_budget = match capabilities.max_context_tokens {
        Some(context_window) if reserved_request_tokens >= context_window => {
            return Err(
                "provider context window cannot fit the reserved output and request overhead"
                    .to_string(),
            );
        }
        Some(context_window) => Some(context_window - reserved_request_tokens),
        None => None,
    };

    Ok(ContextBudget {
        model_context_window: capabilities.max_context_tokens,
        reserved_output_tokens,
        fixed_overhead_tokens,
        developer_instruction_tokens,
        tool_tokens,
        message_framing_tokens,
        input_token_budget,
    })
}

fn model_request_fits_context(
    tools: &[ModelToolSchema],
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    provider_reasoning_history: &[ProviderReasoningReplay],
    budget: &ContextBudget,
) -> bool {
    let request_tokens = model_request_token_count(
        tools,
        messages,
        tool_result_occurrences,
        provider_reasoning_history,
        budget,
    );
    budget
        .model_context_window
        .is_none_or(|context_window| request_tokens <= context_window)
}

fn model_request_token_count(
    tools: &[ModelToolSchema],
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    provider_reasoning_history: &[ProviderReasoningReplay],
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
    let tool_result_accounting = tool_result_context_token_adjustment_with_provider(
        messages,
        tool_result_occurrences,
        provider_reasoning_history,
    );
    let message_framing = u32::try_from(messages.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    payload_tokens
        .saturating_add(provider_reasoning_token_count(provider_reasoning_history))
        .saturating_add(tool_result_accounting)
        .saturating_add(budget.reserved_output_tokens)
        .saturating_add(message_framing)
        .saturating_add(budget.fixed_overhead_tokens)
}

fn provider_reasoning_token_count(history: &[ProviderReasoningReplay]) -> u32 {
    serde_json::to_string(history).map_or(u32::MAX, |payload| approximate_token_count(&payload))
}

fn provider_reasoning_tool_call_ids(
    history: &[ProviderReasoningReplay],
    messages: &[ModelMessage],
) -> BTreeSet<String> {
    messages
        .iter()
        .filter(|message| message.role == ModelRole::Assistant)
        .flat_map(|message| {
            message
                .tool_calls
                .iter()
                .filter(|call| {
                    history
                        .iter()
                        .any(|replay| replay.has_tool_call_id(&call.tool_call_id))
                })
                .map(|call| call.tool_call_id.clone())
        })
        .collect()
}

/// 将真实追加顺序中的 tool occurrence 与安全结果 accounting 对齐；压缩占位消息不重复计入。
fn tool_result_context_token_adjustment_with_provider(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    provider_reasoning_history: &[ProviderReasoningReplay],
) -> u32 {
    let private_call_ids = provider_reasoning_tool_call_ids(provider_reasoning_history, messages);
    let Some(occurrences) = tool_result_message_occurrences_with_private_call_ids(
        messages,
        tool_result_occurrences,
        &private_call_ids,
    ) else {
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

#[cfg(test)]
fn tool_result_context_token_adjustment(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
) -> u32 {
    tool_result_context_token_adjustment_with_provider(messages, tool_result_occurrences, &[])
}

#[derive(Debug, Clone, Copy)]
struct ToolResultMessageOccurrence {
    assistant_index: usize,
    tool_index: Option<usize>,
    result_index: usize,
    visibility: ToolResultVisibility,
}

/// 按 occurrence 顺序验证当前 tool message 与结果的一一绑定。
#[cfg(test)]
fn tool_result_message_occurrences(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
) -> Option<Vec<ToolResultMessageOccurrence>> {
    tool_result_message_occurrences_with_private_call_ids(
        messages,
        tool_result_occurrences,
        &BTreeSet::new(),
    )
}

fn tool_result_message_occurrences_with_private_call_ids(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    private_call_ids: &BTreeSet<String>,
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
    let result_call_ids = result_occurrences
        .iter()
        .filter_map(|(index, _)| {
            tool_result_occurrences
                .get(*index)
                .map(|occurrence| occurrence.result().tool_call_id.as_str())
        })
        .collect::<BTreeSet<_>>();
    if messages.iter().any(|message| {
        message.role == ModelRole::Assistant
            && message.tool_calls.iter().any(|call| {
                !result_call_ids.contains(call.tool_call_id.as_str())
                    && !private_call_ids.contains(&call.tool_call_id)
            })
    }) || messages.iter().any(|message| {
        message.role == ModelRole::Tool
            && message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| !result_call_ids.contains(id) && !private_call_ids.contains(id))
    }) {
        return None;
    }
    // Historical provider-private tool transcripts are present in the request messages but have
    // no current-turn ToolResultOccurrence.  Only pair the occurrence-owned calls here; private
    // messages still contribute their serialized payload tokens above.
    let assistant_calls = assistant_calls
        .into_iter()
        .filter(|(_, _, call_id)| result_call_ids.contains(call_id))
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
        .filter_map(|(index, message)| {
            (message.role == ModelRole::Tool
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| result_call_ids.contains(id)))
            .then_some(index)
        })
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
        &state.provider_reasoning_history,
        budget,
    );
    if budget
        .model_context_window
        .is_none_or(|context_window| before_tokens <= context_window)
    {
        return None;
    }
    let private_call_ids =
        provider_reasoning_tool_call_ids(&state.provider_reasoning_history, &state.messages);
    let occurrences = tool_result_message_occurrences_with_private_call_ids(
        &state.messages,
        &state.tool_result_occurrences,
        &private_call_ids,
    )?;

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

    let retained_reasoning_history = state
        .provider_reasoning_history
        .iter()
        .filter(|replay| replay.is_bound_to_messages(&messages))
        .cloned()
        .collect::<Vec<_>>();
    let after_tokens = model_request_token_count(
        tools,
        &messages,
        &tool_result_occurrences,
        &retained_reasoning_history,
        budget,
    );
    if after_tokens >= before_tokens
        || budget
            .model_context_window
            .is_some_and(|context_window| after_tokens > context_window)
    {
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
    checkpoint.validate_for_restore()?;
    let checkpoint_state = checkpoint.state.clone();
    let pending_tool_call = pending.pending_tool_call();
    if checkpoint_state.thread_id != input.thread_id {
        return Err("approval checkpoint thread mismatch".to_string());
    }
    if checkpoint_state.turn_id != input.turn_id {
        return Err("approval checkpoint turn mismatch".to_string());
    }
    if checkpoint_state.project_instructions_digest != input.project_instructions_digest {
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
    let last_message = checkpoint_state
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
    if canonical_model_call.tool_call_id != pending_call.tool_call_id {
        return Err(
            "approval checkpoint assistant tool-call id does not match pending call".to_string(),
        );
    }
    if canonical_model_call.tool_name != pending_call.tool_name {
        return Err(
            "approval checkpoint assistant tool-call name does not match pending call".to_string(),
        );
    }
    if !checkpoint_arguments_equivalent(&canonical_model_call.arguments, &pending_call.arguments) {
        return Err(
            "approval checkpoint assistant tool-call arguments do not match pending call"
                .to_string(),
        );
    }
    let used_approval_grants = checkpoint_state
        .used_approval_grants
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if used_approval_grants.contains(&pending_tool_call.request_id) {
        return Err("approval checkpoint consumed the pending grant".to_string());
    }
    let tool_result_occurrences = checkpoint_state.tool_result_occurrences.clone();
    let checkpoint_history_messages =
        &checkpoint_state.messages[..checkpoint_state.messages.len() - 1];
    let private_call_ids = provider_reasoning_tool_call_ids(
        &checkpoint_state.provider_reasoning_history,
        checkpoint_history_messages,
    );
    if tool_result_message_occurrences_with_private_call_ids(
        checkpoint_history_messages,
        &tool_result_occurrences,
        &private_call_ids,
    )
    .is_none()
    {
        return Err("approval checkpoint tool result occurrence bindings are invalid".to_string());
    }
    let derived_completion = restore_completion_from_history_with_provider(
        checkpoint_history_messages,
        &tool_result_occurrences,
        &checkpoint_state.completion,
        &checkpoint_state.provider_reasoning_history,
    )?;
    if derived_completion != checkpoint_state.completion {
        return Err("approval checkpoint completion state mismatch".to_string());
    }
    let seen_tool_call_fingerprints = checkpoint_state
        .seen_tool_call_fingerprints
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut state = AgentLoopState::new(checkpoint_state.messages, input.max_turns.max(1), None);
    state.provider_reasoning_history = checkpoint_state.provider_reasoning_history;
    state.tool_result_occurrences = tool_result_occurrences;
    state.used_approval_grants = used_approval_grants;
    state.prior_approval_count = checkpoint_state.approval_count;
    state.completion = derived_completion;
    state.repair_state = checkpoint_state.repair_state;
    state.repair_attempts = checkpoint_state.repair_attempts;
    state.repair_cycles = checkpoint_state.repair_cycles;
    state.last_completion_error = checkpoint_state.last_completion_error;
    state.recovery_metrics = checkpoint_state.recovery_metrics;
    state.model_usage = checkpoint_state.model_usage;
    state.provider_attempts = checkpoint_state.provider_attempts;
    state.context_trace = checkpoint_state.context_trace;
    state.seen_tool_call_fingerprints = seen_tool_call_fingerprints;
    state.last_repair_failure = checkpoint_state.last_repair_failure;
    Ok((state, checkpoint_state.model_turns))
}

/// Rebuild completion from checkpoint occurrences while preserving the post-input invalidation
/// boundary. Omitted occurrences still prove workspace revision, but cannot prove terminal
/// verification after a newer user input unless a later visible/compacted assistant call binds it.
#[cfg(test)]
fn restore_completion_from_history(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    checkpoint_completion: &CompletionTracker,
) -> Result<CompletionTracker, String> {
    restore_completion_from_history_with_provider(
        messages,
        tool_result_occurrences,
        checkpoint_completion,
        &[],
    )
}

fn restore_completion_from_history_with_provider(
    messages: &[ModelMessage],
    tool_result_occurrences: &[ToolResultOccurrence],
    checkpoint_completion: &CompletionTracker,
    provider_reasoning_history: &[ProviderReasoningReplay],
) -> Result<CompletionTracker, String> {
    let private_call_ids = provider_reasoning_tool_call_ids(provider_reasoning_history, messages);
    let occurrences = tool_result_message_occurrences_with_private_call_ids(
        messages,
        tool_result_occurrences,
        &private_call_ids,
    )
    .ok_or_else(|| "turn checkpoint tool result occurrence bindings are invalid".to_string())?;
    let mut derived = CompletionTracker::default();
    for occurrence in tool_result_occurrences {
        if occurrence.result().error_code.as_deref() != Some("not_executed_due_to_user_input") {
            derived.observe(occurrence.result());
        }
    }
    if !derived.is_consistent() {
        return Err("turn checkpoint derived workspace revision state is invalid".to_string());
    }

    let user_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == ModelRole::User).then_some(index))
        .collect::<Vec<_>>();
    if user_indices.len() > 1 {
        let latest_user_index = *user_indices.last().expect("user count checked");
        let has_post_input_occurrence = occurrences
            .iter()
            .any(|occurrence| occurrence.assistant_index > latest_user_index);
        if has_post_input_occurrence {
            derived.invalidate_after_user_input();
            for occurrence in occurrences
                .iter()
                .filter(|occurrence| occurrence.assistant_index > latest_user_index)
            {
                let result = tool_result_occurrences
                    .get(occurrence.result_index)
                    .ok_or_else(|| {
                        "turn checkpoint tool result occurrence index is invalid".to_string()
                    })?
                    .result();
                if result.error_code.as_deref() == Some("not_executed_due_to_user_input") {
                    continue;
                }
                if result.tool_name == TOOL_COMMAND
                    && result.ok
                    && let Some(observation) = result.workspace_observation()
                    && observation.mutation() == WorkspaceMutation::Unchanged
                    && let Some(revision) = observation.revision()
                {
                    derived.record_terminal_command_observation(
                        successful_command_scope_digest(result),
                        revision,
                        1,
                    );
                }
            }
        } else if checkpoint_completion.requires_post_input_verification() {
            // Context compaction may omit the post-input assistant/tool messages. The persisted
            // reducer bit is then the only remaining proof; retain its fail-closed state.
            derived.invalidate_after_user_input();
        }
    } else if checkpoint_completion.requires_post_input_verification() {
        // Context compaction may retain only the newest user message. The persisted reducer bit
        // still proves that this message superseded the previous terminal evidence.
        derived.invalidate_after_user_input();
    }

    if !derived.is_consistent() || derived != *checkpoint_completion {
        return Err("turn checkpoint completion state mismatch".to_string());
    }
    Ok(derived)
}

/// Restore the shared state of an ordinary turn checkpoint. Unlike approval restore, this path
/// intentionally has no pending tool call and therefore cannot replay an interrupted side effect.
fn restore_turn_checkpoint(
    input: &AgentLoopInput,
    checkpoint: &TurnCheckpoint,
) -> Result<(AgentLoopState, u32), String> {
    checkpoint.validate_for_restore()?;
    let checkpoint_state = checkpoint.state.clone();
    if checkpoint_state.thread_id != input.thread_id {
        return Err("turn checkpoint thread mismatch".to_string());
    }

    if checkpoint_state.turn_id != input.turn_id {
        return Err("turn checkpoint turn mismatch".to_string());
    }
    if checkpoint_state.resume_attempt != input.resume_attempt {
        return Err("turn checkpoint resume attempt mismatch".to_string());
    }
    if checkpoint_state.project_instructions_digest != input.project_instructions_digest {
        return Err("turn checkpoint project instructions digest mismatch".to_string());
    }
    if checkpoint_state.messages.is_empty() {
        return Err("turn checkpoint messages are missing".to_string());
    }
    let private_call_ids = provider_reasoning_tool_call_ids(
        &checkpoint_state.provider_reasoning_history,
        &checkpoint_state.messages,
    );
    if tool_result_message_occurrences_with_private_call_ids(
        &checkpoint_state.messages,
        &checkpoint_state.tool_result_occurrences,
        &private_call_ids,
    )
    .is_none()
    {
        return Err("turn checkpoint tool result occurrence bindings are invalid".to_string());
    }
    let derived_completion = restore_completion_from_history_with_provider(
        &checkpoint_state.messages,
        &checkpoint_state.tool_result_occurrences,
        &checkpoint_state.completion,
        &checkpoint_state.provider_reasoning_history,
    )?;
    let mut state = AgentLoopState::new(checkpoint_state.messages, input.max_turns.max(1), None);
    // Provider-private reasoning replay is part of the durable turn snapshot. Restore it before
    // the next model request so a process restart cannot silently drop opaque provider state.
    state.provider_reasoning_history = checkpoint_state.provider_reasoning_history;
    state.tool_result_occurrences = checkpoint_state.tool_result_occurrences;
    state.used_approval_grants = checkpoint_state.used_approval_grants.into_iter().collect();
    state.prior_approval_count = checkpoint_state.approval_count;
    state.completion = derived_completion;
    state.repair_state = checkpoint_state.repair_state;
    state.repair_attempts = checkpoint_state.repair_attempts;
    state.repair_cycles = checkpoint_state.repair_cycles;
    state.last_completion_error = checkpoint_state.last_completion_error;
    state.recovery_metrics = checkpoint_state.recovery_metrics;
    state.model_usage = checkpoint_state.model_usage;
    state.provider_attempts = checkpoint_state.provider_attempts;
    state.context_trace = checkpoint_state.context_trace;
    state.seen_tool_call_fingerprints = checkpoint_state
        .seen_tool_call_fingerprints
        .into_iter()
        .collect();
    state.last_repair_failure = checkpoint_state.last_repair_failure;
    Ok((state, checkpoint_state.model_turns))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_observation_metrics: Option<SafeWorkspaceObservationMetrics>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum SafeAuditTimeout {
    Seconds(u64),
    Unknown(&'static str),
}

#[derive(Debug, Clone, Serialize)]
struct SafeWorkspaceObservationMetrics {
    stage: &'static str,
    revision: Option<u64>,
    contract: &'static str,
    before: SafeWorkspaceObservationPhaseMetrics,
    after: SafeWorkspaceObservationPhaseMetrics,
}

#[derive(Debug, Clone, Serialize)]
struct SafeWorkspaceObservationPhaseMetrics {
    mode: &'static str,
    duration_ms: u64,
    entries_read: u64,
    content_bytes_read: u64,
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
        workspace_observation_metrics: safe_workspace_observation_metrics(object),
    };
    serde_json::to_value(projection).expect("safe audit projection serializes")
}

fn safe_workspace_observation_metrics(
    object: Option<&serde_json::Map<String, Value>>,
) -> Option<SafeWorkspaceObservationMetrics> {
    let metrics = object?.get("workspace_observation_metrics")?.as_object()?;
    if metrics.get("stage").and_then(Value::as_str) != Some("agent_command")
        || metrics.get("contract").and_then(Value::as_str)
            != Some("windows_workspace_observation/v1")
    {
        return None;
    }
    Some(SafeWorkspaceObservationMetrics {
        stage: "agent_command",
        revision: metrics.get("revision").and_then(Value::as_u64),
        contract: "windows_workspace_observation/v1",
        before: safe_workspace_observation_phase(metrics.get("before")?)?,
        after: safe_workspace_observation_phase(metrics.get("after")?)?,
    })
}

fn safe_workspace_observation_phase(value: &Value) -> Option<SafeWorkspaceObservationPhaseMetrics> {
    let phase = value.as_object()?;
    let mode = match phase.get("mode").and_then(Value::as_str)? {
        "full" => "full",
        "incremental" => "incremental",
        "reused" => "reused",
        _ => return None,
    };
    Some(SafeWorkspaceObservationPhaseMetrics {
        mode,
        duration_ms: phase.get("duration_ms")?.as_u64()?,
        entries_read: phase.get("entries_read")?.as_u64()?,
        content_bytes_read: phase.get("content_bytes_read")?.as_u64()?,
    })
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

/// Binding may materialize absent optional object fields as `null`; those two JSON forms have the
/// same executable meaning, while every non-null value and every array position remains exact.
fn checkpoint_arguments_equivalent(model: &Value, pending: &Value) -> bool {
    match (model, pending) {
        (Value::Object(model), Value::Object(pending)) => {
            model.iter().all(|(key, value)| {
                pending.get(key).map_or(value.is_null(), |pending_value| {
                    checkpoint_arguments_equivalent(value, pending_value)
                })
            }) && pending.iter().all(|(key, value)| {
                model.get(key).map_or(value.is_null(), |model_value| {
                    checkpoint_arguments_equivalent(model_value, value)
                })
            })
        }
        (Value::Array(model), Value::Array(pending)) => {
            model.len() == pending.len()
                && model
                    .iter()
                    .zip(pending)
                    .all(|(left, right)| checkpoint_arguments_equivalent(left, right))
        }
        _ => model == pending,
    }
}

fn validate_workspace_change_summary(
    call: &ModelToolCall,
    result: &ToolResult,
) -> Result<(), String> {
    let producer_summary = result.workspace_change_summary().ok_or_else(|| {
        "workspace mutation did not provide a trusted changed-files and diff digest summary"
            .to_string()
    })?;
    producer_summary
        .validate()
        .map_err(|error| format!("workspace mutation change summary is invalid: {error}"))?;
    if !is_sha256_fingerprint(&producer_summary.diff_digest) {
        return Err("workspace mutation diff digest is invalid".to_string());
    }
    let requested_paths = mutation_paths_from_call(call)?;
    let observed_paths = producer_summary.changed_files.clone();
    if !requested_paths.is_empty() {
        let requested = requested_paths.iter().collect::<BTreeSet<_>>();
        let observed = observed_paths.iter().collect::<BTreeSet<_>>();
        if requested != observed {
            return Err(
                "workspace mutation changed-files summary does not match the executed call"
                    .to_string(),
            );
        }
    }
    let mut normalized = BTreeSet::new();
    for path in observed_paths {
        if !is_bounded_workspace_relative_path(&path) {
            return Err(
                "workspace mutation changed path is outside the bounded relative scope".to_string(),
            );
        }
        normalized.insert(path);
    }
    if normalized.is_empty() {
        return Err("workspace mutation did not provide a changed path summary".to_string());
    }
    Ok(())
}

fn is_bounded_workspace_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.chars().count() <= MAX_BOUNDED_TEXT_CHARS
        && !path.contains('\0')
        && !std::path::Path::new(path).is_absolute()
        && std::path::Path::new(path).components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

fn mutation_paths_from_call(call: &ModelToolCall) -> Result<Vec<String>, String> {
    let paths = match call.tool_name.as_str() {
        TOOL_PATCH => serde_json::from_value::<WorkspacePatch>(call.arguments.clone())
            .map_err(|_| "workspace patch arguments are invalid".to_string())?
            .changes
            .into_iter()
            .map(|change| change.path)
            .collect(),
        TOOL_COMMAND => Vec::new(),
        _ => return Err("workspace mutation summary has an unexpected tool".to_string()),
    };
    Ok(paths)
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

fn repair_reason_text(reason: AgentRepairReason) -> &'static str {
    match reason {
        AgentRepairReason::VerificationFailed => "revision-bound verification failed",
        AgentRepairReason::ToolFailure => "repairable tool failure",
        AgentRepairReason::RevisionConflict => "workspace revision conflict",
    }
}

fn bounded_repair_text(value: &str) -> String {
    value.chars().take(MAX_REPAIR_CONTEXT_CHARS).collect()
}

/// Project only the already-redacted public tool result payload into bounded repair evidence.
fn safe_tool_result_evidence(result: &ToolResult) -> String {
    let payload = result.to_message_payload();
    let value = payload
        .get("preview")
        .or_else(|| payload.get("content"))
        .or_else(|| payload.get("error_code"));
    let evidence = value
        .and_then(|value| serde_json::to_string(value).ok())
        .unwrap_or_else(|| if result.ok { "ok" } else { "failed" }.to_string());
    bounded_repair_text(&evidence)
}

fn safe_repair_tool_name(result: &ToolResult) -> String {
    if is_provider_history_validation_rejection(result) {
        return bounded_repair_text(&result.tool_name);
    }
    result
        .to_message_payload()
        .get("tool_name")
        .and_then(Value::as_str)
        .map(bounded_repair_text)
        .unwrap_or_else(|| "tool".to_string())
}

fn tool_result_command_scope_digest(result: &ToolResult) -> Option<&str> {
    result
        .result_id
        .as_deref()
        .filter(|digest| is_sha256_fingerprint(digest))
        .or_else(|| {
            result
                .audit_metadata()
                .and_then(|metadata| metadata.get("command_scope_digest"))
                .and_then(Value::as_str)
                .filter(|digest| is_sha256_fingerprint(digest))
        })
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

fn patch_tool_input(arguments: &Value) -> Result<WorkspacePatch, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
}

fn command_tool_input(arguments: &Value) -> Result<CommandToolInput, AgentLoopToolError> {
    serde_json::from_value(arguments.clone()).map_err(invalid_tool_arguments)
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

fn invalid_tool_arguments_result(
    call: &ModelToolCall,
    error: ToolInputValidationError,
    spec: Option<&ToolSpec>,
) -> ToolResult {
    let envelope = tool_call_request(&provider_history_rejected_tool_call(call));
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
                "summary": "The requested tool is not callable. Choose a registered tool from the current tools schema or revise the approach.",
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
        }),
    )
}

fn invalid_tool_arguments(error: serde_json::Error) -> AgentLoopToolError {
    AgentLoopToolError::InvalidArguments(error.to_string())
}

fn recoverable_tool_response_validation(
    response: &ModelTurnResponse,
    validation_errors: &[String],
) -> bool {
    !response.tool_calls.is_empty()
        && !validation_errors.is_empty()
        && validation_errors.iter().all(|error| {
            matches!(
                error.as_str(),
                "invalid_json" | "schema_mismatch" | "tool_call_arguments_must_be_object"
            )
        })
        && response
            .tool_calls
            .iter()
            .all(|call| !call.tool_call_id.trim().is_empty() && !call.tool_name.trim().is_empty())
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
            "tool path is outside the workspace; use cwd \".\" or another workspace-relative directory and do not prefix commands with a guessed absolute workspace path"
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
            workspace_change_summary: result.workspace_change_summary().cloned(),
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
            workspace_change_summary: result.workspace_change_summary().cloned(),
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
            workspace_change_summary: large.workspace_change_summary().cloned(),
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
            "workspace_observation_metrics": {
                "stage": "agent_command",
                "revision": 3,
                "contract": "windows_workspace_observation/v1",
                "before": {
                    "mode": "reused",
                    "duration_ms": 2,
                    "entries_read": 1,
                    "content_bytes_read": 0,
                },
                "after": {
                    "mode": "incremental",
                    "duration_ms": 4,
                    "entries_read": 5,
                    "content_bytes_read": 6,
                },
            },
        });

        let projected = project_audit_event(&raw);
        assert_eq!(projected["sandbox_mode"], "workspace_write");
        assert_eq!(
            projected["command_scope_digest"],
            raw["command_scope_digest"]
        );
        assert_eq!(
            projected["workspace_observation_metrics"],
            raw["workspace_observation_metrics"]
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
        let mutation = ToolResult::summary("patch", TOOL_PATCH, true, "changed")
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
        let mutation = ToolResult::summary("patch", TOOL_PATCH, true, "changed")
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
            &ToolResult::summary("patch", TOOL_PATCH, true, "changed")
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
    fn follow_up_invalidates_terminal_evidence_across_restore_and_compaction() {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let patch = ToolResult::summary("patch", TOOL_PATCH, true, "changed")
            .with_workspace_observation(WorkspaceObservation::changed(revision));
        let mut command = ToolResult::summary("command", TOOL_COMMAND, true, "verified");
        command.result_id = Some(digest.to_string());
        command = command.with_workspace_observation(WorkspaceObservation::unchanged(revision));
        let occurrences = vec![
            ToolResultOccurrence::new(patch.clone(), ToolResultVisibility::Visible),
            ToolResultOccurrence::new(command.clone(), ToolResultVisibility::Visible),
        ];
        let mut messages = vec![
            ModelMessage::text(ModelRole::User, "initial task"),
            assistant_tool_message_for("patch", TOOL_PATCH),
            tool_message("patch", "changed"),
            assistant_tool_message_for("command", TOOL_COMMAND),
            tool_message("command", "verified"),
        ];
        messages.push(ModelMessage::text(ModelRole::User, "follow-up task"));

        let mut expected = CompletionTracker::default();
        expected.observe(&patch);
        expected.observe(&command);
        expected.invalidate_after_user_input();
        assert!(!expected.verification_satisfied());
        let restored = restore_completion_from_history(&messages, &occurrences, &expected)
            .expect("follow-up restore");
        assert_eq!(restored, expected);
        assert!(!restored.verification_satisfied());

        messages.push(assistant_tool_message_for("command-2", TOOL_COMMAND));
        messages.push(tool_message("command-2", "verified again"));
        let mut command_after_follow_up = command.clone();
        command_after_follow_up.tool_call_id = "command-2".to_string();
        let mut occurrences_after_follow_up = occurrences.clone();
        occurrences_after_follow_up.push(ToolResultOccurrence::new(
            command_after_follow_up.clone(),
            ToolResultVisibility::Visible,
        ));
        let mut expected_after_follow_up = expected.clone();
        expected_after_follow_up.observe(&command_after_follow_up);
        let restored_after_follow_up = restore_completion_from_history(
            &messages,
            &occurrences_after_follow_up,
            &expected_after_follow_up,
        )
        .expect("post-follow-up restore");
        assert!(restored_after_follow_up.verification_satisfied());

        let compacted_messages = vec![ModelMessage::text(ModelRole::User, "follow-up task")];
        let compacted_occurrences = occurrences
            .into_iter()
            .map(|occurrence| {
                ToolResultOccurrence::new(occurrence.into_result(), ToolResultVisibility::Omitted)
            })
            .collect::<Vec<_>>();
        let compacted =
            restore_completion_from_history(&compacted_messages, &compacted_occurrences, &expected)
                .expect("compacted follow-up restore");
        assert_eq!(compacted, expected);
        assert!(!compacted.verification_satisfied());
    }

    fn assistant_tool_message_for(tool_call_id: &str, tool_name: &str) -> ModelMessage {
        ModelMessage::assistant_tool_calls(vec![ModelToolCall {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments: json!({}),
            raw_arguments: "{}".to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        }])
    }

    fn tool_message(tool_call_id: &str, content: &str) -> ModelMessage {
        let mut message = ModelMessage::text(ModelRole::Tool, content);
        message.tool_call_id = Some(tool_call_id.to_string());
        message
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
    fn workspace_invalid_input_keeps_its_boundary_error_semantics() {
        let output = workspace_tool_failure(AgentLoopToolError::Workspace(
            WorkspaceToolError::InvalidInput("tool backend is unavailable".to_string()),
        ));
        let envelope = ToolCallRequest::new("call_1", singularity_tools::READ_TOOL, "{}");
        let result = ToolResult::from_result(&envelope, &output);
        let message = model_turn::tool_result_message(&result);

        assert_eq!(result.error_code.as_deref(), Some("invalid_tool_input"));
        assert_eq!(result.failure_kind, Some(ToolFailureKind::Input));
        assert!(!message.content.contains("rejection_kind"));
        assert!(!message.content.contains("placeholder_non_callable"));
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

    #[test]
    fn prepare_seed_messages_preserves_compaction_summary_and_replaces_leading_developer() {
        let summary = ModelMessage::text(
            ModelRole::Developer,
            format!("{COMPACTION_SUMMARY_PREFIX}\"omitted_message_count\":3}}"),
        );
        let old_leading = ModelMessage::text(ModelRole::Developer, "old leading instructions");
        let repair = ModelMessage::text(
            ModelRole::Developer,
            format!("{REPAIR_STATE_INSTRUCTIONS} repair_context=..."),
        );
        let history_user = ModelMessage::text(ModelRole::User, "history user");
        let seed = HistoricalModelContext {
            messages: vec![
                old_leading.clone(),
                summary.clone(),
                repair.clone(),
                history_user.clone(),
            ],
            provider_reasoning_history: Vec::new(),
            tool_result_occurrences: Vec::new(),
        };
        let input = AgentLoopInput::new("thread_1", "turn_2", "current user");
        let messages = prepare_seed_messages(&seed, &input, 1, "current user");

        // 唯一 leading 为新的 developer 指令；compaction summary 保留；repair 剔除。
        assert_eq!(
            messages[0].role,
            ModelRole::Developer,
            "new leading developer"
        );
        assert!(messages[0].content.contains(AGENT_DEVELOPER_INSTRUCTIONS));
        assert!(messages.contains(&summary), "compaction summary preserved");
        assert!(
            !messages
                .iter()
                .any(|message| message.content == old_leading.content),
            "old leading replaced"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.content.starts_with(REPAIR_STATE_INSTRUCTIONS)),
            "repair feedback dropped"
        );
        assert!(
            messages.iter().any(|message| {
                message.role == ModelRole::User && message.content == "current user"
            }),
            "current user appended"
        );
        // 顺序：leading 必须在 compaction summary 之前。
        let leading_index = messages
            .iter()
            .position(|message| message.content.contains(AGENT_DEVELOPER_INSTRUCTIONS))
            .expect("leading developer");
        let summary_index = messages
            .iter()
            .position(|message| message.content == summary.content)
            .expect("summary");
        assert!(leading_index < summary_index, "leading precedes summary");
    }
}
