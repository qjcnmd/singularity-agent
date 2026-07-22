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
    pub(super) verification_plan: Option<super::VerificationPlanState>,
    pub(super) verification_change: Option<super::VerificationChangeSummary>,
    pub(super) verification_failure_history: Vec<String>,
    pub(super) repair_plan: Option<super::RepairPlanState>,
    pub(super) repair_attempts: u32,
    pub(super) final_review_verdict: Option<super::FinalReviewVerdict>,
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
    #[serde(default)]
    verification_plan: Option<super::VerificationPlanState>,
    #[serde(default)]
    verification_change: Option<super::VerificationChangeSummary>,
    #[serde(default)]
    verification_failure_history: Vec<String>,
    #[serde(default)]
    repair_plan: Option<super::RepairPlanState>,
    repair_attempts: u32,
    #[serde(default)]
    final_review_verdict: Option<super::FinalReviewVerdict>,
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
            verification_plan: checkpoint.verification_plan.clone(),
            verification_change: checkpoint.verification_change.clone(),
            verification_failure_history: checkpoint.verification_failure_history.clone(),
            repair_plan: checkpoint.repair_plan.clone(),
            repair_attempts: checkpoint.repair_attempts,
            final_review_verdict: checkpoint.final_review_verdict,
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
            verification_plan: wire.verification_plan,
            verification_change: wire.verification_change,
            verification_failure_history: wire.verification_failure_history,
            repair_plan: wire.repair_plan,
            repair_attempts: wire.repair_attempts,
            final_review_verdict: wire.final_review_verdict,
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
        if self
            .verification_failure_history
            .iter()
            .any(|failure| failure.trim().is_empty() || failure.chars().count() > 128)
        {
            return Err("approval checkpoint verification failure history is invalid".to_string());
        }
        if let Some(change) = &self.verification_change
            && (!super::is_sha256_fingerprint(&change.diff_digest)
                || change.changed_paths.is_empty()
                || change.changed_paths.len() > super::MAX_VERIFICATION_REQUIREMENTS
                || Some(change.revision) != self.completion.workspace_revision
                || change
                    .changed_paths
                    .iter()
                    .any(|path| !super::is_bounded_workspace_relative_path(path)))
        {
            return Err("approval checkpoint verification change summary is invalid".to_string());
        }
        if self.completion.workspace_mutated() && self.verification_change.is_none() {
            return Err("approval checkpoint workspace change summary is missing".to_string());
        }
        if self.completion.workspace_mutated() {
            let producer = self
                .tool_result_occurrences
                .iter()
                .rev()
                .find(|occurrence| {
                    occurrence
                        .result()
                        .workspace_observation()
                        .is_some_and(|observation| {
                            observation.mutation() == singularity_tools::WorkspaceMutation::Changed
                        })
                })
                .and_then(|occurrence| {
                    let result = occurrence.result();
                    Some((
                        result.workspace_observation()?.revision()?,
                        result.workspace_change_summary()?,
                    ))
                })
                .ok_or_else(|| {
                    "approval checkpoint mutation evidence is missing from its tool occurrence"
                        .to_string()
                })?;
            let producer_paths = producer
                .1
                .changed_files
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let change = self.verification_change.as_ref().expect("checked above");
            if producer.0 != change.revision
                || producer.1.diff_digest != change.diff_digest
                || producer_paths.len() != producer.1.changed_files.len()
                || producer_paths.into_iter().collect::<Vec<_>>() != change.changed_paths
            {
                return Err(
                    "approval checkpoint verification change summary is not bound to its tool occurrence"
                        .to_string(),
                );
            }
        }
        if let Some(plan) = &self.verification_plan {
            plan.plan.validate().map_err(|error| {
                format!("approval checkpoint verification plan is invalid: {error}")
            })?;
            if let Some(revision) = plan.revision
                && Some(revision) != self.completion.workspace_revision
            {
                return Err(
                    "approval checkpoint verification plan revision binding is invalid".to_string(),
                );
            }
            if self.completion.workspace_mutated() && plan.revision.is_none() {
                return Err(
                    "approval checkpoint verification plan revision binding is missing".to_string(),
                );
            }
        }
        if let Some(repair) = &self.repair_plan {
            if repair.plan.attempt == 0
                || repair.plan.attempt > repair.plan.max_attempts.saturating_add(1)
            {
                return Err("approval checkpoint repair plan budget is invalid".to_string());
            }
            if repair.plan.max_attempts == 0
                || repair.plan.max_attempts > super::MAX_REPAIR_PLAN_ATTEMPTS
            {
                return Err("approval checkpoint repair plan bound is invalid".to_string());
            }
            if !repair.signature.is_empty() && !is_sha256_fingerprint(&repair.signature) {
                return Err("approval checkpoint repair plan signature is invalid".to_string());
            }
            if repair.plan.required_revision
                != self
                    .verification_plan
                    .as_ref()
                    .and_then(|plan| plan.revision)
                || repair.plan.required_check_count != self.completion.required_command_count()
            {
                return Err("approval checkpoint repair plan binding is invalid".to_string());
            }
        }
        if self.repair_attempts > super::MAX_REPAIR_PLAN_ATTEMPTS.saturating_add(1) {
            return Err("approval checkpoint repair attempt ledger is invalid".to_string());
        }
        if self.repair_attempts != self.recovery_metrics.repair_attempt_count {
            return Err("approval checkpoint repair attempt metrics are inconsistent".to_string());
        }
        let observed_failed_repairs = u32::try_from(
            self.tool_result_occurrences
                .iter()
                .filter(|occurrence| {
                    let result = occurrence.result();
                    !result.ok && super::is_repairable_tool_result(result)
                })
                .count(),
        )
        .unwrap_or(u32::MAX);
        if self.repair_attempts < observed_failed_repairs {
            return Err(
                "approval checkpoint repair attempt ledger is below observed failures".to_string(),
            );
        }
        if self
            .repair_plan
            .as_ref()
            .is_some_and(|repair| repair.plan.attempt != self.repair_attempts)
        {
            return Err("approval checkpoint repair attempt ledger is not monotonic".to_string());
        }
        if self.final_review_verdict == Some(super::FinalReviewVerdict::Accept)
            && (!self.completion.allows_final()
                || self.plan.as_ref().is_some_and(|plan| !plan.is_completed()))
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
