//! Typed approval checkpoint codec.
//!
//! This module owns the persistence boundary and validates request, tool-call, occurrence,
//! completion, and history bindings before a checkpoint can be resumed.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::{
    ModelMessage, ModelToolCall, ModelToolParseStatus, ModelUsage, ProviderAttemptMetadata,
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
    #[serde(default)]
    pub(super) final_review_verdict: Option<super::FinalReviewVerdict>,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnCheckpoint {
    pub(super) state: CheckpointState,
    pub(super) pending_tool_calls: Vec<PendingToolCall>,
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
}

impl ApprovalCheckpoint {
    pub(super) fn validate_serialized(&self) -> Result<(), String> {
        self.pending_tool_call.validate()?;
        self.state
            .validate_serialized(APPROVAL_CHECKPOINT_VERSION, true)
    }
}

impl TurnCheckpoint {
    fn validate_serialized(&self) -> Result<(), String> {
        self.state
            .validate_serialized(super::TURN_CHECKPOINT_VERSION, false)?;
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
        if self.final_review_verdict == Some(super::FinalReviewVerdict::Accept)
            && !self.completion.allows_final()
        {
            return Err(
                "approval checkpoint accepted review lacks completion evidence".to_string(),
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
