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
    ToolFailureKind, ToolInputValidationError, ToolOutput, ToolResult, ToolSpec, WorkspacePatch,
    WorkspaceRevision, WorkspaceToolError, WorkspaceToolExecutor, WorkspaceTools,
    approximate_token_count, command_script_scope_digest_with_policy,
};
use thiserror::Error;

mod checkpoint;
mod context;
mod model_turn;
mod observation;
mod occurrence;
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
            )?;
        }
        self.append_user_inputs(inputs)
    }

    fn with_pending_tool_failure(
        mut self,
        error_code: &str,
        summary: &str,
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
            let fingerprint = pending
                .to_model_tool_call()
                .map(|call| tool_call_fingerprint(&call))
                .map_err(|error| {
                    format!("turn checkpoint pending tool call is invalid: {error}")
                })?;
            if !self
                .state
                .completed_tool_call_fingerprints
                .iter()
                .any(|known| known == &fingerprint)
            {
                self.state
                    .completed_tool_call_fingerprints
                    .push(fingerprint);
            }
            let mut result = ToolResult::summary(tool_call_id, tool_name, false, summary);
            result.error_code = Some(error_code.to_string());
            result.failure_kind = Some(ToolFailureKind::Cancelled);
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
        self.state.messages.extend(
            inputs
                .iter()
                .map(|input| ModelMessage::text(ModelRole::User, input.clone())),
        );
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
    ToolResultObservation,
};
pub use occurrence::{
    ToolResultOccurrence, ToolResultVisibility, successful_command_scope_digest,
    terminal_command_scope_digests,
};
use tool_occurrence::*;

#[cfg(test)]
use occurrence::ToolResultOccurrenceWire;
#[cfg(test)]
use singularity_tools::{ToolCallRequest, WorkspaceObservation};

const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 16;
const MAX_PARALLEL_READ_TOOL_CALLS: u32 = 8;
/// Approval checkpoints carry transcript, occurrence and approval facts only.
const APPROVAL_CHECKPOINT_VERSION: u32 = 8;
/// Ordinary turn checkpoints carry transcript and pending-call facts only.
const TURN_CHECKPOINT_VERSION: u32 = 7;
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
const MAX_BOUNDED_TEXT_CHARS: usize = 512;
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

/// 从 loop 派生的公开运行状态，包括门禁证据和安全诊断信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunStatus {
    pub status: AgentStatus,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub audit_events: Vec<Value>,
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
            final_answer: None,
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            audit_events: Vec::new(),
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

/// Compaction summary 消息的固定 JSON 标记，跨轮 seed 保留该 Developer 消息。
///
/// 使用 `contains` 而非前缀：serde_json 默认 BTreeMap 按字母序序列化，
/// `"type"` 键位于对象中部，前缀匹配对真实格式恒不成立。
const COMPACTION_SUMMARY_MARKER: &str = "\"type\":\"agent_context_compaction\"";

/// 从跨轮 seed 组装新 Turn 的初始模型消息。
///
/// Replace the historical leading developer block while preserving compaction summaries.
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
            || message.content.contains(COMPACTION_SUMMARY_MARKER)
        {
            // 旧 leading block 被替换；compaction summary 紧跟 leading 且必须保留。
            messages.push(message);
            break;
        }
    }
    messages.extend(seed_messages);
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
            final_answer: self.final_answer.clone(),
            model_turns: self.model_turns,
            tool_calls: self.tool_calls,
            approval_count: self.approval_count,
            audit_events: audit_events_from_tool_results(&self.tool_results),
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
    execution_facts: ExecutionFacts,
    model_usage: ModelUsage,
    provider_attempts: ProviderAttemptMetadata,
    seen_tool_call_fingerprints: BTreeSet<String>,
    /// Logical tool fingerprints that already reached a terminal outcome. This is persisted in
    /// checkpoints so approval/process recovery cannot count a repeated fingerprint twice.
    completed_tool_call_fingerprints: BTreeSet<String>,
    /// Claims made by the current in-flight batch; unlike the completed ledger, this is not
    /// persisted because suspended approval has not reached a terminal outcome.
    first_attempt_claims: BTreeSet<String>,
    model_turn_limit: u32,
    context_trace: Option<AgentContextTrace>,
    provider_protocol_contract: Option<ProviderProtocolContract>,
    provider_capability_metadata: Option<ProviderCapabilityMetadata>,
}

/// Execution facts retained across tool calls and checkpoint restore.
///
/// Carries the latest trusted workspace revision so a resumed workspace executor can reject stale
/// mutations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExecutionFacts {
    workspace_revision: Option<WorkspaceRevision>,
}

impl ExecutionFacts {
    fn observe(&mut self, result: &ToolResult) {
        if let Some(observation) = result.workspace_observation()
            && let Some(revision) = observation.revision()
        {
            self.workspace_revision = Some(revision);
        }
    }
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
            execution_facts: ExecutionFacts::default(),
            model_usage: ModelUsage::default(),
            provider_attempts: ProviderAttemptMetadata::default(),
            seen_tool_call_fingerprints: BTreeSet::new(),
            completed_tool_call_fingerprints: BTreeSet::new(),
            first_attempt_claims: BTreeSet::new(),
            model_turn_limit,
            context_trace,
            provider_protocol_contract: None,
            provider_capability_metadata: None,
        }
    }

    fn finish(
        self,
        status: AgentStatus,
        _completed: bool,
        final_answer: Option<String>,
        model_turns: u32,
        error: Option<String>,
    ) -> AgentLoopResult {
        self.finish_with_model_error(status, _completed, final_answer, model_turns, error, None)
    }

    fn finish_with_model_error(
        self,
        status: AgentStatus,
        _completed: bool,
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
        AgentLoopResult {
            status,
            final_answer,
            model_turns,
            tool_calls: tool_results.len() as u32,
            approval_count,
            pending_approvals: self.pending_approvals,
            tool_results,
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
            model_usage: self.model_usage.clone(),
            provider_attempts,
            context_trace: self.context_trace.clone(),
            seen_tool_call_fingerprints: self.seen_tool_call_fingerprints.iter().cloned().collect(),
            completed_tool_call_fingerprints: self
                .completed_tool_call_fingerprints
                .iter()
                .cloned()
                .collect(),
        }
    }

    fn observe_model_tool_call(
        &mut self,
        call: &ModelToolCall,
        allowed_tool_names: &[String],
    ) -> (String, bool, bool) {
        let fingerprint = tool_call_fingerprint(call);
        self.seen_tool_call_fingerprints.insert(fingerprint.clone());
        let invalid = call.parse_status != ModelToolParseStatus::Valid
            || !call.arguments.is_object()
            || call.tool_name.trim().is_empty()
            || !allowed_tool_names
                .iter()
                .any(|tool_name| tool_name == &call.tool_name);
        let _ = invalid;
        let first_attempt = self.claim_first_attempt(&fingerprint);
        (fingerprint, invalid, first_attempt)
    }

    /// Claim the first terminal-attempt slot for one logical tool fingerprint in this run.
    fn claim_first_attempt(&mut self, fingerprint: &str) -> bool {
        self.seen_tool_call_fingerprints
            .insert(fingerprint.to_string());
        !self.completed_tool_call_fingerprints.contains(fingerprint)
            && self.first_attempt_claims.insert(fingerprint.to_string())
    }

    /// Record a terminal outcome before projecting its End event and checkpoint.
    fn record_terminal_tool_call(&mut self, fingerprint: &str) {
        self.completed_tool_call_fingerprints
            .insert(fingerprint.to_string());
    }

    fn observe_tool_result(&mut self, tool_result: &ToolResult, _tool_call_fingerprint: &str) {
        self.execution_facts.observe(tool_result);
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

/// Callbacks projected at typed runtime and durable checkpoint boundaries.
///
/// The event callback receives only bounded `AgentLoopEvent` values. The checkpoint callback is
/// invoked after a checkpoint has been assembled and before the loop crosses that boundary.
pub struct AgentLoopCallbacks<'a> {
    pub on_event: Option<&'a mut AgentLoopEventCallback<'a>>,
    pub on_checkpoint: Option<&'a mut AgentLoopCheckpointCallback<'a>>,
}

impl<'a> AgentLoopCallbacks<'a> {
    /// Construct callbacks for a caller that only consumes runtime events.
    pub fn events(on_event: &'a mut AgentLoopEventCallback<'a>) -> Self {
        Self {
            on_event: Some(on_event),
            on_checkpoint: None,
        }
    }

    /// Construct callbacks for callers that persist every durable boundary.
    pub fn events_and_checkpoints(
        on_event: &'a mut AgentLoopEventCallback<'a>,
        on_checkpoint: &'a mut AgentLoopCheckpointCallback<'a>,
    ) -> Self {
        Self {
            on_event: Some(on_event),
            on_checkpoint: Some(on_checkpoint),
        }
    }

    /// Construct a callback set with no external projections.
    pub const fn none() -> Self {
        Self {
            on_event: None,
            on_checkpoint: None,
        }
    }
}

/// The durable continuation source used by the single resume entry point.
pub enum AgentContinuation<'a> {
    Turn(&'a TurnCheckpoint),
    Approval(&'a PendingApprovalOccurrence),
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

    /// Run a new turn until completion, approval pause, cancellation, or a typed failure.
    pub fn run(
        &self,
        input: &AgentLoopInput,
        callbacks: AgentLoopCallbacks<'_>,
    ) -> AgentLoopResult {
        let AgentLoopCallbacks {
            on_event,
            on_checkpoint,
        } = callbacks;
        self.run_internal_with_checkpoints(input, on_event, on_checkpoint)
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
            state.context_trace = seed.context_trace.clone();
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

    /// Resume one validated durable continuation without replaying a completed tool call.
    pub fn resume(
        &self,
        input: &AgentLoopInput,
        continuation: AgentContinuation<'_>,
        callbacks: AgentLoopCallbacks<'_>,
    ) -> AgentLoopResult {
        let AgentLoopCallbacks {
            on_event,
            on_checkpoint,
        } = callbacks;
        match continuation {
            AgentContinuation::Turn(checkpoint) => {
                self.resume_turn_internal(input, checkpoint, on_event, on_checkpoint)
            }
            AgentContinuation::Approval(pending) => {
                self.resume_pending_approval_internal(input, pending, on_event, on_checkpoint)
            }
        }
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
                .bind_checkpoint_workspace_revision(state.execution_facts.workspace_revision)
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
                let fingerprint = tool_call_fingerprint(&call);
                let first_attempt = state.claim_first_attempt(&fingerprint);
                occurrences.push(ModelToolOccurrence {
                    fingerprint,
                    invalid_was_observed: false,
                    context: tool_occurrence_context(
                        input,
                        &call,
                        model_turn_offset,
                        u32::try_from(ordinal).unwrap_or(u32::MAX),
                    ),
                    call,
                });
                if let Some(occurrence) = occurrences.last_mut() {
                    occurrence.context.first_attempt = first_attempt;
                }
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
            state.context_trace = seed.context_trace.clone();
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
        if model_turn_offset >= max_turns {
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
        for turn_index in model_turn_offset..max_turns {
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index, None);
            }
            if !matches!(
                self.emit_checkpoint_event(
                    input,
                    &state,
                    TurnCheckpointPhase::BeforeModelRequest,
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
            let tool_view = match model_tool_view(&self.tool_broker, capabilities, max_tool_calls) {
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
                    return state.finish(AgentStatus::Failed, false, None, turn_index, Some(error));
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
            let request =
                model_turn_request(input, budget, turn_index, &state, tool_view, capabilities);
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
            let recoverable_tool_validation =
                recoverable_tool_response_validation(&response, &validation.errors);
            if !validation.valid && !recoverable_tool_validation {
                let first_attempts = response
                    .tool_calls
                    .iter()
                    .map(|call| state.observe_model_tool_call(call, &provider_tool_names).2)
                    .collect::<Vec<_>>();
                if emit_rejected_tool_calls(
                    &mut on_event,
                    input,
                    &response.tool_calls,
                    turn_index,
                    &first_attempts,
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
                let first_attempts = response
                    .tool_calls
                    .iter()
                    .map(|call| state.observe_model_tool_call(call, &provider_tool_names).2)
                    .collect::<Vec<_>>();
                if emit_rejected_tool_calls(
                    &mut on_event,
                    input,
                    &response.tool_calls,
                    turn_index,
                    &first_attempts,
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
                let first_attempts = response
                    .tool_calls
                    .iter()
                    .map(|call| state.observe_model_tool_call(call, &provider_tool_names).2)
                    .collect::<Vec<_>>();
                if emit_rejected_tool_calls(
                    &mut on_event,
                    input,
                    &response.tool_calls,
                    turn_index,
                    &first_attempts,
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
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        actual_model_turns,
                        Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    );
                }
                {
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
            for occurrence in &mut tool_occurrences {
                let (fingerprint, invalid_was_observed, first_attempt) =
                    state.observe_model_tool_call(&occurrence.call, &execution_tool_names);
                occurrence.fingerprint = fingerprint;
                occurrence.invalid_was_observed = invalid_was_observed;
                occurrence.context.first_attempt = first_attempt;
            }
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
        state.finish(
            AgentStatus::Failed,
            false,
            None,
            actual_model_turns,
            Some("max turns exceeded".to_string()),
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
                .bind_checkpoint_workspace_revision(state.execution_facts.workspace_revision)
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
        let first_attempt = state.claim_first_attempt(&tool_call_fingerprint);
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
            context: {
                let mut context =
                    tool_occurrence_context(input, &call, model_turn_offset.saturating_sub(1), 0);
                context.first_attempt = first_attempt;
                context
            },
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
            let recorded = state
                .tool_result_occurrences
                .last()
                .expect("recorded approval tool occurrence");
            if emit_event(
                on_event,
                tool_result_event(
                    &occurrences
                        .first()
                        .expect("single approval occurrence is present")
                        .context,
                    ToolCallStatus::ApprovalRequired,
                    recorded,
                ),
            )
            .is_err()
            {
                return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
            }
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
        _invalid_was_observed: bool,
        _state: &mut AgentLoopState,
    ) -> PreparedToolCall {
        if execution_call.parse_status != ModelToolParseStatus::Valid {
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
                Ok(input) => command_workspace_tool_failure(
                    &input,
                    error.into(),
                    &self.policy.profile,
                    Some(false),
                ),
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

    /// Commit typed tool results and decide whether the next model turn is safe.
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
        let mut fatal_error = None;
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
            let feedback_safe = matches!(
                result.failure_kind,
                Some(
                    ToolFailureKind::Input
                        | ToolFailureKind::Visibility
                        | ToolFailureKind::Capability
                        | ToolFailureKind::Policy
                        | ToolFailureKind::PermissionProfile
                        | ToolFailureKind::WorkspaceBoundary
                        | ToolFailureKind::ProtectedPath
                        | ToolFailureKind::Approval
                        | ToolFailureKind::Execution
                )
            ) || (batch_rejected
                && result.failure_kind == Some(ToolFailureKind::Approval));
            if !result.ok && !feedback_safe && fatal_error.is_none() {
                fatal_error = Some(
                    result
                        .error_code
                        .clone()
                        .unwrap_or_else(|| "tool_execution_failed".to_string()),
                );
            }
            if result.workspace_observation().is_some_and(|observation| {
                observation.mutation() == singularity_tools::WorkspaceMutation::Unknown
            }) && fatal_error.is_none()
            {
                fatal_error = Some("workspace_observation_unknown".to_string());
            }
            state.observe_tool_result(&result, &prepared.fingerprint);
            let changed = result.workspace_observation().is_some_and(|observation| {
                observation.mutation() == singularity_tools::WorkspaceMutation::Changed
            });
            if changed {
                let Some(revision) = result
                    .workspace_observation()
                    .and_then(|observation| observation.revision())
                else {
                    return ToolBatchControl::Failed(
                        "workspace mutation revision is missing".to_string(),
                    );
                };
                match validate_workspace_change_summary(&prepared.call, &result) {
                    Ok(()) => {}
                    Err(error) => {
                        return ToolBatchControl::Failed(error);
                    }
                }
                state.execution_facts.workspace_revision = Some(revision);
            }
            state.append_visible_tool_result(result.clone());
            let recorded = state
                .tool_result_occurrences
                .last()
                .expect("recorded tool occurrence");
            let status = tool_result_status(&prepared, recorded.result(), batch_rejected);
            if emit_event(
                on_event,
                tool_result_event(&occurrence.context, status, recorded),
            )
            .is_err()
            {
                return ToolBatchControl::Failed(EVENT_SINK_FAILURE_ERROR.to_string());
            }
            state.record_terminal_tool_call(&prepared.fingerprint);
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
        if self.is_cancelled(input) {
            ToolBatchControl::Cancelled
        } else if let Some(error_code) = fatal_error {
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
                        Err(error) => {
                            command_workspace_tool_failure(&input, error.into(), profile, None)
                        }
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
    })
    .to_string()
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
    state.model_usage = checkpoint_state.model_usage;
    state.provider_attempts = checkpoint_state.provider_attempts;
    state.context_trace = checkpoint_state.context_trace;
    state.seen_tool_call_fingerprints = seen_tool_call_fingerprints;
    state.completed_tool_call_fingerprints = checkpoint_state
        .completed_tool_call_fingerprints
        .into_iter()
        .collect();
    state.execution_facts.workspace_revision = state
        .tool_result_occurrences
        .iter()
        .filter_map(|occurrence| occurrence.result().workspace_observation()?.revision())
        .max_by_key(|revision| revision.value());
    Ok((state, checkpoint_state.model_turns))
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
    let mut state = AgentLoopState::new(checkpoint_state.messages, input.max_turns.max(1), None);
    // Provider-private reasoning replay is part of the durable turn snapshot. Restore it before
    // the next model request so a process restart cannot silently drop opaque provider state.
    state.provider_reasoning_history = checkpoint_state.provider_reasoning_history;
    state.tool_result_occurrences = checkpoint_state.tool_result_occurrences;
    state.used_approval_grants = checkpoint_state.used_approval_grants.into_iter().collect();
    state.prior_approval_count = checkpoint_state.approval_count;
    state.model_usage = checkpoint_state.model_usage;
    state.provider_attempts = checkpoint_state.provider_attempts;
    state.context_trace = checkpoint_state.context_trace;
    state.seen_tool_call_fingerprints = checkpoint_state
        .seen_tool_call_fingerprints
        .into_iter()
        .collect();
    state.completed_tool_call_fingerprints = checkpoint_state
        .completed_tool_call_fingerprints
        .into_iter()
        .collect();
    state.execution_facts.workspace_revision = state
        .tool_result_occurrences
        .iter()
        .filter_map(|occurrence| occurrence.result().workspace_observation()?.revision())
        .max_by_key(|revision| revision.value());
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
        final_answer: None,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        pending_approvals: Vec::new(),
        tool_results: Vec::new(),
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
    executor_started: Option<bool>,
) -> ToolOutput {
    let mut output = workspace_tool_failure(error);
    let (sandbox_mode, network_access) = effective_command_policy(profile);
    let mut audit = json!({
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
    if let Some(executor_started) = executor_started {
        audit["executor_started"] = json!(executor_started);
    }
    output.metadata["audit"] = audit;
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
mod tool_first_attempt_tests {
    use super::*;

    #[test]
    fn terminal_tool_fingerprint_is_claimed_once_and_repeated_calls_are_not_first_attempts() {
        let mut state = AgentLoopState::new(Vec::new(), 1, None);
        let fingerprint = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(state.claim_first_attempt(fingerprint));
        assert!(!state.claim_first_attempt(fingerprint));
        state.record_terminal_tool_call(fingerprint);
        assert!(!state.claim_first_attempt(fingerprint));
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
            // 与 serde_json 真实序列化一致（BTreeMap 字母序，"type" 在对象中部）。
            serde_json::json!({
                "type": "agent_context_compaction",
                "omitted_message_count": 3
            })
            .to_string(),
        );
        let old_leading = ModelMessage::text(ModelRole::Developer, "old leading instructions");
        let history_user = ModelMessage::text(ModelRole::User, "history user");
        let seed = HistoricalModelContext {
            messages: vec![old_leading.clone(), summary.clone(), history_user.clone()],
            provider_reasoning_history: Vec::new(),
            tool_result_occurrences: Vec::new(),
            context_trace: None,
        };
        let input = AgentLoopInput::new("thread_1", "turn_2", "current user");
        let messages = prepare_seed_messages(&seed, &input, 1, "current user");

        // The current leading developer block and compaction summary are preserved.
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
