//! Typed approval checkpoint codec.
//!
//! This module owns the persistence boundary and validates request, tool-call, occurrence,
//! completion, and history bindings before a checkpoint can be resumed.

use std::collections::BTreeSet;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::{
    ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus, ModelUsage,
    ProviderAttemptMetadata, ProviderReasoningReplay,
};
use singularity_policy::{ApprovalRequest, PermissionResource, ToolId};
use singularity_tools::BoundToolCall;

use super::completion::{
    CompletionTracker, RepairFailureState, ToolResultOccurrence, ToolResultVisibility,
};
use super::context::AgentContextTrace;
use super::{
    APPROVAL_CHECKPOINT_VERSION, AgentLoopInput, AgentRecoveryMetrics, approval_request_id,
    is_sha256_fingerprint,
};

/// approval 暂停运行期间保留的规范化可执行 tool call 数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PendingToolCall {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: ToolId,
    pub raw_arguments: String,
    pub resources: Vec<PermissionResource>,
}

impl PendingToolCall {
    /// 从已经完成注册表和工作区绑定的调用创建待执行记录。
    pub(super) fn new(input: &AgentLoopInput, call: &ModelToolCall, bound: &BoundToolCall) -> Self {
        Self {
            request_id: approval_request_id(input, call),
            tool_call_id: call.tool_call_id.clone(),
            tool_name: bound.tool_id.clone(),
            raw_arguments: call.raw_arguments.clone(),
            resources: bound.resources.clone(),
        }
    }

    pub(super) fn to_model_tool_call(&self) -> Result<ModelToolCall, serde_json::Error> {
        let arguments: Value = serde_json::from_str(&self.raw_arguments)?;
        Ok(ModelToolCall {
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.as_str().to_string(),
            raw_arguments: self.raw_arguments.clone(),
            arguments,
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        })
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.request_id.trim().is_empty() || self.tool_call_id.trim().is_empty() {
            return Err("approval checkpoint pending tool call identity is missing".to_string());
        }
        if self.tool_name.as_str().trim().is_empty() {
            return Err("approval checkpoint pending tool name is missing".to_string());
        }
        if matches!(self.tool_name.as_str(), "update_plan" | "edit") {
            return Err("approval checkpoint contains a retired tool call".to_string());
        }
        serde_json::from_str::<Value>(&self.raw_arguments).map_err(|error| {
            format!("approval checkpoint pending tool arguments are invalid: {error}")
        })?;
        Ok(())
    }
}

/// 一个待处理 approval occurrence 的唯一 typed 所有者。
///
/// request 与 checkpoint 不再通过多个同下标数组关联；checkpoint 内部持有同一份规范化
/// executable tool call，构造和恢复时会集中校验三者绑定关系。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApprovalOccurrence {
    request: ApprovalRequest,
    checkpoint: ApprovalCheckpoint,
}

impl PendingApprovalOccurrence {
    pub(super) fn new(
        request: ApprovalRequest,
        checkpoint: ApprovalCheckpoint,
    ) -> Result<Self, String> {
        checkpoint.validate_serialized()?;
        let occurrence = Self {
            request,
            checkpoint,
        };
        occurrence.validate_binding()?;
        Ok(occurrence)
    }

    /// 从持久化 checkpoint 解码一个完整的 typed approval occurrence。
    pub fn from_checkpoint_payload(
        request: ApprovalRequest,
        payload: &Value,
    ) -> Result<Self, String> {
        Self::new(request, ApprovalCheckpoint::decode(payload)?)
    }

    /// 返回绑定的 approval request。
    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// 返回 checkpoint 唯一持有的规范化可执行 tool call。
    pub fn pending_tool_call(&self) -> &PendingToolCall {
        self.checkpoint.pending_tool_call()
    }

    /// 返回版本化 typed checkpoint。
    pub fn checkpoint(&self) -> &ApprovalCheckpoint {
        &self.checkpoint
    }

    /// Encode only when crossing the persistence boundary.
    pub fn encode_checkpoint(&self) -> Result<Value, String> {
        self.checkpoint.encode()
    }

    pub(super) fn validate_binding(&self) -> Result<(), String> {
        let pending = self.checkpoint.pending_tool_call();
        if self.request.request_id != pending.request_id {
            return Err("approval occurrence request mismatch".to_string());
        }
        if self.request.thread_id != self.checkpoint.state.thread_id
            || self.request.turn_id != self.checkpoint.state.turn_id
        {
            return Err("approval occurrence thread or turn mismatch".to_string());
        }
        if self.request.tool_call_id.as_deref() != Some(pending.tool_call_id.as_str()) {
            return Err("approval occurrence tool call id mismatch".to_string());
        }
        if self.request.action != pending.tool_name || self.request.resources != pending.resources {
            return Err("approval occurrence tool binding mismatch".to_string());
        }
        Ok(())
    }
}

/// 版本化 approval checkpoint 的 opaque typed 状态。
///
/// JSON 只通过 [`Self::encode`] 与 [`Self::decode`] 进入持久化边界；业务代码不能直接读取
/// JSON 字段，也不能绕过集中校验构造 checkpoint。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalCheckpoint {
    pub(super) pending_tool_call: PendingToolCall,
    pub(super) state: CheckpointState,
}

/// The state shared by approval and ordinary turn checkpoints.
///
/// Keeping this as one private value is intentional: validation and resume must not drift between
/// the approval continuation path and a process-restart continuation path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointState {
    pub(super) checkpoint_version: u32,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) project_instructions_digest: Option<String>,
    pub(super) messages: Vec<ModelMessage>,
    #[serde(default)]
    pub(super) provider_reasoning_history: Vec<ProviderReasoningReplay>,
    pub(super) tool_result_occurrences: Vec<ToolResultOccurrence>,
    pub(super) used_approval_grants: Vec<String>,
    pub(super) approval_count: u32,
    pub(super) model_turns: u32,
    #[serde(default)]
    pub(super) resume_attempt: u32,
    pub(super) completion: CompletionTracker,
    #[serde(default)]
    pub(super) repair_state: Option<super::RepairState>,
    pub(super) repair_attempts: u32,
    #[serde(default)]
    pub(super) repair_cycles: Vec<super::RepairCycleRecord>,
    pub(super) last_completion_error: Option<String>,
    pub(super) recovery_metrics: AgentRecoveryMetrics,
    pub(super) model_usage: ModelUsage,
    pub(super) provider_attempts: ProviderAttemptMetadata,
    pub(super) context_trace: Option<AgentContextTrace>,
    pub(super) seen_tool_call_fingerprints: Vec<String>,
    pub(super) last_repair_failure: Option<RepairFailureState>,
}

/// A durable boundary for a non-approval turn and its next safe action.
///
/// Validated calls are retained only until execution starts. Once the store records `Running`,
/// owner loss becomes `Unknown` and this checkpoint can no longer authorize automatic execution.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnCheckpoint {
    pub(super) state: CheckpointState,
    pub(super) pending_tool_calls: Vec<PendingToolCall>,
}

/// A private, provider-facing transcript fragment recovered from one completed historical turn.
///
/// The public conversation stores only the final assistant text.  This value keeps the exact
/// assistant tool-call and tool-result messages that belong to the opaque reasoning replay, while
/// binding the fragment to the public assistant item that owns the turn.  It is intentionally not
/// serializable: checkpoints remain the single durable source and this projection is rebuilt on
/// demand after a process restart.
#[derive(Clone, PartialEq)]
pub struct ProviderHistorySegment {
    assistant_item_id: String,
    messages: Vec<ModelMessage>,
    provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl fmt::Debug for ProviderHistorySegment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHistorySegment")
            .field("assistant_item_id", &self.assistant_item_id)
            .field("message_count", &self.messages.len())
            .field(
                "reasoning_item_count",
                &self.provider_reasoning_history.len(),
            )
            .finish()
    }
}

impl ProviderHistorySegment {
    fn new(
        assistant_item_id: impl Into<String>,
        messages: Vec<ModelMessage>,
        provider_reasoning_history: Vec<ProviderReasoningReplay>,
    ) -> Result<Self, String> {
        let assistant_item_id = assistant_item_id.into();
        if assistant_item_id.trim().is_empty() {
            return Err("provider history segment assistant item identity is missing".to_string());
        }
        if messages.is_empty() || provider_reasoning_history.is_empty() {
            return Err("provider history segment payload is incomplete".to_string());
        }
        Ok(Self {
            assistant_item_id,
            messages,
            provider_reasoning_history,
        })
    }

    /// Return the public assistant item that owns this private fragment.
    pub(crate) fn assistant_item_id(&self) -> &str {
        &self.assistant_item_id
    }

    /// Return whether two segments are attached to the same public assistant item.
    pub fn same_assistant_item(&self, other: &Self) -> bool {
        self.assistant_item_id == other.assistant_item_id
    }

    /// Return the exact provider-facing tool-call/result messages in original order.
    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }

    /// Return the opaque provider reasoning items paired with these tool calls.
    pub(crate) fn provider_reasoning_history(&self) -> &[ProviderReasoningReplay] {
        &self.provider_reasoning_history
    }

    /// Return true when this segment contains a replay binding that overlaps
    /// another segment.  Only the conflict result crosses the app boundary.
    pub fn conflicts_with(&self, other: &Self) -> bool {
        self.provider_reasoning_history.iter().any(|replay| {
            other.provider_reasoning_history.iter().any(|candidate| {
                self.messages
                    .iter()
                    .flat_map(|message| message.tool_calls.iter())
                    .any(|call| {
                        replay.has_tool_call_id(&call.tool_call_id)
                            && candidate.has_tool_call_id(&call.tool_call_id)
                    })
            })
        })
    }

    /// Return true when the segment carries no public assistant projection.
    pub fn has_replay(&self) -> bool {
        !self.provider_reasoning_history.is_empty()
    }

    /// Approximate the private payload cost for context selection and request budgeting.
    pub fn token_count(&self) -> u32 {
        let message_tokens = serde_json::to_string(&self.messages).map_or(u32::MAX, |payload| {
            singularity_tools::approximate_token_count(&payload)
        });
        let reasoning_tokens = serde_json::to_string(&self.provider_reasoning_history)
            .map_or(u32::MAX, |payload| {
                singularity_tools::approximate_token_count(&payload)
            });
        message_tokens.saturating_add(reasoning_tokens)
    }
}

/// Durable-boundary notifications consumed by the process owner. The callback is invoked only at
/// complete input, validated-tool-call, or complete ToolResult batch boundaries; it never receives
/// a partial streamed assistant response.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnCheckpointPhase {
    Initial,
    BeforeModelRequest {
        finalization_only: bool,
    },
    ToolCallsReady {
        pending_tool_calls: Vec<PendingToolCall>,
    },
    ToolResultsCommitted {
        tool_call_ids: Vec<String>,
    },
    ModelResponseCommitted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnCheckpointEvent {
    pub phase: TurnCheckpointPhase,
    pub checkpoint: TurnCheckpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalCheckpointWire {
    #[serde(flatten)]
    pending_tool_call: PendingToolCall,
    #[serde(flatten)]
    state: CheckpointState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnCheckpointWire {
    pending_tool_calls: Vec<PendingToolCall>,
    #[serde(flatten)]
    state: CheckpointState,
}

#[derive(Debug, Deserialize)]
struct CheckpointVersion {
    checkpoint_version: u32,
}

impl From<&ApprovalCheckpoint> for ApprovalCheckpointWire {
    fn from(checkpoint: &ApprovalCheckpoint) -> Self {
        Self {
            pending_tool_call: checkpoint.pending_tool_call.clone(),
            state: checkpoint.state.clone(),
        }
    }
}

impl ApprovalCheckpoint {
    /// Encode a validated checkpoint at the persistence boundary.
    pub fn encode(&self) -> Result<Value, String> {
        self.validate_serialized()?;
        serde_json::to_value(ApprovalCheckpointWire::from(self))
            .map_err(|error| format!("approval checkpoint serialization failed: {error}"))
    }

    /// Decode a persisted checkpoint and reject every non-current version.
    pub fn decode(payload: &Value) -> Result<Self, String> {
        let version: CheckpointVersion = serde_json::from_value(payload.clone())
            .map_err(|error| format!("invalid approval checkpoint version: {error}"))?;
        let checkpoint = match version.checkpoint_version {
            APPROVAL_CHECKPOINT_VERSION => {
                let wire: ApprovalCheckpointWire = serde_json::from_value(payload.clone())
                    .map_err(|error| format!("invalid approval checkpoint: {error}"))?;
                Self::from_wire(wire)
            }
            _ => return Err("unsupported approval checkpoint version".to_string()),
        };
        checkpoint.validate_serialized()?;
        Ok(checkpoint)
    }

    /// 返回 checkpoint 唯一持有的规范化可执行 tool call。
    pub fn pending_tool_call(&self) -> &PendingToolCall {
        &self.pending_tool_call
    }

    fn from_wire(wire: ApprovalCheckpointWire) -> Self {
        Self {
            pending_tool_call: wire.pending_tool_call,
            state: wire.state,
        }
    }
}

impl TurnCheckpoint {
    /// Encode a validated ordinary turn checkpoint at the persistence boundary.
    pub fn encode(&self) -> Result<Value, String> {
        self.validate_serialized()?;
        serde_json::to_value(TurnCheckpointWire {
            pending_tool_calls: self.pending_tool_calls.clone(),
            state: self.state.clone(),
        })
        .map_err(|error| format!("turn checkpoint serialization failed: {error}"))
    }

    /// Decode and fail closed on old, future, or unknown checkpoint fields.
    pub fn decode(payload: &Value) -> Result<Self, String> {
        let version: CheckpointVersion = serde_json::from_value(payload.clone())
            .map_err(|error| format!("invalid turn checkpoint version: {error}"))?;
        if version.checkpoint_version != super::TURN_CHECKPOINT_VERSION {
            return Err("unsupported turn checkpoint version".to_string());
        }
        let wire: TurnCheckpointWire = serde_json::from_value(payload.clone())
            .map_err(|error| format!("invalid turn checkpoint: {error}"))?;
        let checkpoint = Self {
            state: wire.state,
            pending_tool_calls: wire.pending_tool_calls,
        };
        checkpoint.validate_serialized()?;
        Ok(checkpoint)
    }

    pub fn thread_id(&self) -> &str {
        &self.state.thread_id
    }

    pub fn turn_id(&self) -> &str {
        &self.state.turn_id
    }

    pub fn model_turns(&self) -> u32 {
        self.state.model_turns
    }

    /// Return the codec version that must be stored beside this payload.
    pub fn checkpoint_version(&self) -> u32 {
        self.state.checkpoint_version
    }

    /// Return the durable occurrence-identity epoch for this continuation.
    pub fn resume_attempt(&self) -> u32 {
        self.state.resume_attempt
    }

    /// Bind a new persisted continuation epoch before issuing provider requests.
    pub fn with_resume_attempt(mut self, resume_attempt: u32) -> Self {
        self.state.resume_attempt = resume_attempt;
        self
    }

    /// Return canonical validated calls that may run only when no execution became `Unknown`.
    pub fn pending_tool_calls(&self) -> &[PendingToolCall] {
        &self.pending_tool_calls
    }

    /// Return the private provider-bound reasoning state needed for history replay.
    /// Return whether the durable checkpoint contains provider-private replay.
    pub fn has_provider_reasoning_history(&self) -> bool {
        !self.state.provider_reasoning_history.is_empty()
    }

    /// Derive the current turn's private tool transcript from the durable message snapshot.
    ///
    /// Checkpoints carry cumulative replay state because a resumed turn is allowed to retain
    /// earlier history.  Only replay items paired with assistant tool calls after the latest user
    /// message belong to this completed turn; older pairs are recovered from their own turn
    /// checkpoints.  Any globally orphaned replay is rejected before provider content is exposed.
    pub fn provider_history_segment(
        &self,
        assistant_item_id: impl Into<String>,
    ) -> Result<Option<ProviderHistorySegment>, String> {
        let assistant_item_id = assistant_item_id.into();
        if self.state.provider_reasoning_history.is_empty() {
            return Ok(None);
        }
        let latest_user_index = self
            .state
            .messages
            .iter()
            .rposition(|message| message.role == ModelRole::User)
            .ok_or_else(|| "provider history checkpoint has no user message".to_string())?;

        let mut matched_assistant_indices = Vec::new();
        for replay in &self.state.provider_reasoning_history {
            if !replay.is_valid() {
                return Err("provider history replay tool-call identity is invalid".to_string());
            }
            let matches = self
                .state
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| {
                    message.role == ModelRole::Assistant
                        && replay.matches_tool_call_ids(
                            &message
                                .tool_calls
                                .iter()
                                .map(|call| call.tool_call_id.clone())
                                .collect::<Vec<_>>(),
                        )
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(
                    "provider history replay is not bound to a tool-call message".to_string(),
                );
            }
            if matches.len() > 1 {
                return Err("provider history replay has ambiguous tool-call binding".to_string());
            }
            matched_assistant_indices.push(matches[0]);
        }
        if matched_assistant_indices
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != matched_assistant_indices.len()
        {
            return Err("provider history replay has duplicate assistant binding".to_string());
        }

        let current_replay = self
            .state
            .provider_reasoning_history
            .iter()
            .zip(matched_assistant_indices.iter())
            .filter(|(_, index)| **index > latest_user_index)
            .map(|(replay, index)| (replay.clone(), *index))
            .collect::<Vec<_>>();
        if current_replay.is_empty() {
            return Ok(None);
        }

        let current_assistant_indices = current_replay
            .iter()
            .map(|(_, index)| *index)
            .collect::<BTreeSet<_>>();
        let current_call_ids = current_replay
            .iter()
            .filter_map(|(_, index)| self.state.messages.get(*index))
            .flat_map(|message| {
                message
                    .tool_calls
                    .iter()
                    .map(|call| call.tool_call_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let mut selected_indices = BTreeSet::new();
        for assistant_index in current_assistant_indices {
            selected_indices.insert(assistant_index);
            let assistant_calls = self
                .state
                .messages
                .get(assistant_index)
                .map(|message| {
                    message
                        .tool_calls
                        .iter()
                        .map(|call| call.tool_call_id.clone())
                        .collect::<BTreeSet<_>>()
                })
                .ok_or_else(|| "provider history assistant message is missing".to_string())?;
            let mut result_ids = BTreeSet::new();
            for (index, message) in self
                .state
                .messages
                .iter()
                .enumerate()
                .skip(assistant_index + 1)
            {
                if message.role != ModelRole::Tool {
                    if !assistant_calls.is_disjoint(&current_call_ids) {
                        break;
                    }
                    continue;
                }
                let tool_call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                    "provider history tool result identity is missing".to_string()
                })?;
                if assistant_calls.contains(tool_call_id) {
                    selected_indices.insert(index);
                    if !result_ids.insert(tool_call_id.to_string()) {
                        return Err("provider history tool result binding is invalid".to_string());
                    }
                }
            }
            if result_ids != assistant_calls {
                return Err("provider history tool result binding is invalid".to_string());
            }
        }
        let messages = selected_indices
            .into_iter()
            .filter_map(|index| self.state.messages.get(index).cloned())
            .collect::<Vec<_>>();
        let replay = current_replay
            .into_iter()
            .map(|(replay, _)| replay)
            .collect::<Vec<_>>();
        ProviderHistorySegment::new(assistant_item_id, messages, replay).map(Some)
    }
}

impl ApprovalCheckpoint {
    pub(crate) fn validate_for_restore(&self) -> Result<(), String> {
        self.validate_serialized()
    }

    pub(super) fn validate_serialized(&self) -> Result<(), String> {
        self.pending_tool_call.validate()?;
        self.state
            .validate_serialized(APPROVAL_CHECKPOINT_VERSION, true)?;
        validate_provider_replay_bindings(&self.state)
    }
}

impl TurnCheckpoint {
    pub(crate) fn validate_for_restore(&self) -> Result<(), String> {
        self.validate_serialized()
    }

    fn validate_serialized(&self) -> Result<(), String> {
        self.state
            .validate_serialized(super::TURN_CHECKPOINT_VERSION, false)?;
        validate_provider_replay_bindings(&self.state)?;
        let mut ids = BTreeSet::new();
        for pending in &self.pending_tool_calls {
            pending.validate()?;
            if pending.request_id.is_empty() || !ids.insert(&pending.tool_call_id) {
                return Err("turn checkpoint contains duplicate pending tool calls".to_string());
            }
            let bound = self.state.messages.iter().rev().any(|message| {
                message.tool_calls.iter().any(|call| {
                    call.tool_call_id == pending.tool_call_id
                        && call.tool_name == pending.tool_name.as_str()
                })
            });
            if !bound {
                return Err("turn checkpoint pending tool call binding is invalid".to_string());
            }
        }
        Ok(())
    }
}

fn validate_provider_replay_bindings(state: &CheckpointState) -> Result<(), String> {
    let mut replay_call_ids = BTreeSet::new();
    for replay in &state.provider_reasoning_history {
        if !replay.is_valid() || replay.bound_assistant_count(&state.messages) != 1 {
            return Err("provider history replay binding is invalid".to_string());
        }
        for message in state.messages.iter().filter(|message| {
            message.role == ModelRole::Assistant
                && replay.matches_tool_call_ids(
                    &message
                        .tool_calls
                        .iter()
                        .map(|call| call.tool_call_id.clone())
                        .collect::<Vec<_>>(),
                )
        }) {
            for call in &message.tool_calls {
                if !replay_call_ids.insert(call.tool_call_id.clone()) {
                    return Err(
                        "provider history replay tool-call binding is duplicated".to_string()
                    );
                }
            }
        }
    }
    Ok(())
}

impl CheckpointState {
    fn validate_serialized(&self, expected_version: u32, approval: bool) -> Result<(), String> {
        if self.checkpoint_version != expected_version {
            return Err("unsupported approval checkpoint version".to_string());
        }
        if self.thread_id.trim().is_empty() || self.turn_id.trim().is_empty() {
            return Err("approval checkpoint thread or turn is missing".to_string());
        }
        if approval && self.model_turns == 0 {
            return Err("approval checkpoint model-turn offset is invalid".to_string());
        }
        if approval && self.approval_count == 0 {
            return Err("approval checkpoint approval count is invalid".to_string());
        }
        if self.messages.is_empty() {
            return Err("approval checkpoint messages are missing".to_string());
        }
        for replay in &self.provider_reasoning_history {
            if !replay.is_valid() {
                return Err("provider reasoning replay state is invalid".to_string());
            }
        }
        let used_approval_grants = self.used_approval_grants.iter().collect::<BTreeSet<_>>();
        if used_approval_grants.len() != self.used_approval_grants.len() {
            return Err("approval checkpoint contains duplicate grants".to_string());
        }
        let seen_tool_call_fingerprints = self
            .seen_tool_call_fingerprints
            .iter()
            .collect::<BTreeSet<_>>();
        if seen_tool_call_fingerprints.len() != self.seen_tool_call_fingerprints.len()
            || seen_tool_call_fingerprints
                .iter()
                .any(|fingerprint| !is_sha256_fingerprint(fingerprint))
        {
            return Err("approval checkpoint tool-call fingerprint state is invalid".to_string());
        }
        if self.last_repair_failure.as_ref().is_some_and(|failure| {
            failure.consecutive_count == 0 || !is_sha256_fingerprint(&failure.signature)
        }) {
            return Err("approval checkpoint repair state is invalid".to_string());
        }
        if self.provider_attempts.retry_count > self.provider_attempts.attempt_count {
            return Err("approval checkpoint provider attempt state is invalid".to_string());
        }
        if self.context_trace.as_ref().is_some_and(|trace| {
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
        if !self.completion.is_consistent() {
            return Err("approval checkpoint workspace revision state is invalid".to_string());
        }
        for occurrence in &self.tool_result_occurrences {
            let result = occurrence.result();
            if result.workspace_observation().is_some_and(|observation| {
                observation.mutation() == singularity_tools::WorkspaceMutation::Changed
            }) {
                let summary = result.workspace_change_summary().ok_or_else(|| {
                    "approval checkpoint mutation change summary is missing".to_string()
                })?;
                summary.validate().map_err(|error| {
                    format!("approval checkpoint mutation change summary is invalid: {error}")
                })?;
            }
        }
        if let Some(repair) = &self.repair_state {
            if repair.attempt == 0 || repair.attempt > repair.max_attempts.saturating_add(1) {
                return Err("approval checkpoint repair state budget is invalid".to_string());
            }
            if repair.max_attempts == 0 || repair.max_attempts > super::MAX_REPAIR_ATTEMPTS {
                return Err("approval checkpoint repair state bound is invalid".to_string());
            }
            if !repair.signature.is_empty() && !is_sha256_fingerprint(&repair.signature) {
                return Err("approval checkpoint repair state signature is invalid".to_string());
            }
            if repair
                .required_revision
                .is_some_and(|revision| Some(revision) != self.completion.workspace_revision)
            {
                return Err("approval checkpoint repair state binding is invalid".to_string());
            }
        }
        if self.repair_attempts > super::MAX_REPAIR_ATTEMPTS {
            return Err("approval checkpoint repair attempt ledger is invalid".to_string());
        }
        if self.repair_attempts != self.recovery_metrics.repair_attempt_count {
            return Err("approval checkpoint repair attempt metrics are inconsistent".to_string());
        }
        if self.repair_cycles.len() != usize::try_from(self.repair_attempts).unwrap_or(usize::MAX) {
            return Err("approval checkpoint repair cycle ledger is inconsistent".to_string());
        }
        for (index, cycle) in self.repair_cycles.iter().enumerate() {
            let expected_attempt = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if cycle.attempt != expected_attempt
                || !super::is_sha256_fingerprint(&cycle.command_scope_digest)
                || self.repair_cycles[..index]
                    .iter()
                    .any(|previous| previous.revision.value() >= cycle.revision.value())
                || !self.tool_result_occurrences.iter().any(|occurrence| {
                    let result = occurrence.result();
                    result.tool_name == super::TOOL_COMMAND
                        && result.ok == cycle.verification_passed
                        && super::tool_result_command_scope_digest(result)
                            == Some(cycle.command_scope_digest.as_str())
                        && result.workspace_observation().is_some_and(|observation| {
                            observation.mutation()
                                == singularity_tools::WorkspaceMutation::Unchanged
                                && observation.revision() == Some(cycle.revision)
                        })
                })
            {
                return Err("approval checkpoint repair cycle evidence is invalid".to_string());
            }
        }
        if self
            .repair_state
            .as_ref()
            .is_some_and(|repair| repair.attempt != self.repair_attempts.saturating_add(1))
        {
            return Err("approval checkpoint repair attempt ledger is not monotonic".to_string());
        }
        if self.repair_state.is_none() && self.completion.has_unresolved_failures() {
            return Err(
                "approval checkpoint repair state is missing for unresolved failure".to_string(),
            );
        }
        for occurrence in &self.tool_result_occurrences {
            occurrence.validate()?;
        }
        if self
            .context_trace
            .as_ref()
            .map_or(0, |trace| trace.compaction_count)
            == 0
            && self.tool_result_occurrences.iter().any(|occurrence| {
                matches!(
                    occurrence.visibility(),
                    ToolResultVisibility::Compacted | ToolResultVisibility::Omitted
                )
            })
        {
            return Err("approval checkpoint compaction occurrence state is invalid".to_string());
        }
        Ok(())
    }
}
