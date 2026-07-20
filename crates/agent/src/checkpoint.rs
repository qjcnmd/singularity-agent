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
    APPROVAL_CHECKPOINT_VERSION, AgentLoopInput, AgentPlan, AgentRecoveryMetrics,
    approval_request_id, is_sha256_fingerprint,
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
        if self.request.thread_id != self.checkpoint.thread_id
            || self.request.turn_id != self.checkpoint.turn_id
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
    pub(super) checkpoint_version: u32,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) project_instructions_digest: Option<String>,
    pub(super) messages: Vec<ModelMessage>,
    pub(super) tool_result_occurrences: Vec<ToolResultOccurrence>,
    pub(super) used_approval_grants: Vec<String>,
    pub(super) approval_count: u32,
    pub(super) model_turns: u32,
    pub(super) completion: CompletionTracker,
    pub(super) last_completion_error: Option<String>,
    pub(super) plan: Option<AgentPlan>,
    pub(super) plan_update_count: u32,
    pub(super) recovery_metrics: AgentRecoveryMetrics,
    pub(super) model_usage: ModelUsage,
    pub(super) provider_attempts: ProviderAttemptMetadata,
    pub(super) context_trace: Option<AgentContextTrace>,
    pub(super) seen_tool_call_fingerprints: Vec<String>,
    pub(super) last_repair_failure: Option<RepairFailureState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalCheckpointWire {
    #[serde(flatten)]
    pending_tool_call: PendingToolCall,
    checkpoint_version: u32,
    thread_id: String,
    turn_id: String,
    project_instructions_digest: Option<String>,
    messages: Vec<ModelMessage>,
    tool_result_occurrences: Vec<ToolResultOccurrence>,
    used_approval_grants: Vec<String>,
    approval_count: u32,
    model_turns: u32,
    completion: CompletionTracker,
    last_completion_error: Option<String>,
    plan: Option<AgentPlan>,
    plan_update_count: u32,
    recovery_metrics: AgentRecoveryMetrics,
    model_usage: ModelUsage,
    provider_attempts: ProviderAttemptMetadata,
    context_trace: Option<AgentContextTrace>,
    seen_tool_call_fingerprints: Vec<String>,
    last_repair_failure: Option<RepairFailureState>,
}

#[derive(Debug, Deserialize)]
struct CheckpointVersion {
    checkpoint_version: u32,
}

impl From<&ApprovalCheckpoint> for ApprovalCheckpointWire {
    fn from(checkpoint: &ApprovalCheckpoint) -> Self {
        Self {
            pending_tool_call: checkpoint.pending_tool_call.clone(),
            checkpoint_version: checkpoint.checkpoint_version,
            thread_id: checkpoint.thread_id.clone(),
            turn_id: checkpoint.turn_id.clone(),
            project_instructions_digest: checkpoint.project_instructions_digest.clone(),
            messages: checkpoint.messages.clone(),
            tool_result_occurrences: checkpoint.tool_result_occurrences.clone(),
            used_approval_grants: checkpoint.used_approval_grants.clone(),
            approval_count: checkpoint.approval_count,
            model_turns: checkpoint.model_turns,
            completion: checkpoint.completion.clone(),
            last_completion_error: checkpoint.last_completion_error.clone(),
            plan: checkpoint.plan.clone(),
            plan_update_count: checkpoint.plan_update_count,
            recovery_metrics: checkpoint.recovery_metrics.clone(),
            model_usage: checkpoint.model_usage.clone(),
            provider_attempts: checkpoint.provider_attempts.clone(),
            context_trace: checkpoint.context_trace.clone(),
            seen_tool_call_fingerprints: checkpoint.seen_tool_call_fingerprints.clone(),
            last_repair_failure: checkpoint.last_repair_failure.clone(),
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
            checkpoint_version: wire.checkpoint_version,
            thread_id: wire.thread_id,
            turn_id: wire.turn_id,
            project_instructions_digest: wire.project_instructions_digest,
            messages: wire.messages,
            tool_result_occurrences: wire.tool_result_occurrences,
            used_approval_grants: wire.used_approval_grants,
            approval_count: wire.approval_count,
            model_turns: wire.model_turns,
            completion: wire.completion,
            last_completion_error: wire.last_completion_error,
            plan: wire.plan,
            plan_update_count: wire.plan_update_count,
            recovery_metrics: wire.recovery_metrics,
            model_usage: wire.model_usage,
            provider_attempts: wire.provider_attempts,
            context_trace: wire.context_trace,
            seen_tool_call_fingerprints: wire.seen_tool_call_fingerprints,
            last_repair_failure: wire.last_repair_failure,
        }
    }
}

impl ApprovalCheckpoint {
    pub(super) fn validate_serialized(&self) -> Result<(), String> {
        if self.checkpoint_version != APPROVAL_CHECKPOINT_VERSION {
            return Err("unsupported approval checkpoint version".to_string());
        }
        if self.thread_id.trim().is_empty() || self.turn_id.trim().is_empty() {
            return Err("approval checkpoint thread or turn is missing".to_string());
        }
        self.pending_tool_call.validate()?;
        if self.model_turns == 0 {
            return Err("approval checkpoint model-turn offset is invalid".to_string());
        }
        if self.approval_count == 0 {
            return Err("approval checkpoint approval count is invalid".to_string());
        }
        if self.messages.is_empty() {
            return Err("approval checkpoint messages are missing".to_string());
        }
        let used_approval_grants = self.used_approval_grants.iter().collect::<BTreeSet<_>>();
        if used_approval_grants.len() != self.used_approval_grants.len() {
            return Err("approval checkpoint contains duplicate grants".to_string());
        }
        if let Some(plan) = &self.plan {
            plan.validate()
                .map_err(|error| format!("approval checkpoint plan is invalid: {error}"))?;
            if self.plan_update_count == 0 {
                return Err("approval checkpoint plan update count is invalid".to_string());
            }
        } else if self.plan_update_count != 0 {
            return Err("approval checkpoint plan update count is invalid".to_string());
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
