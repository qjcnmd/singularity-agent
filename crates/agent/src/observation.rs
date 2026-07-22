//! AgentLoop 在真实运行边界产出的短生命周期 typed 事件。
//!
//! 这些类型只承载安全计数、摘要、关联标识和状态；它们不保存 prompt、消息、工具参数、
//! 路径、命令或输出，也不拥有持久化和全局收集职责。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use singularity_model::{
    ModelErrorCategory, ProviderApiProtocol, ProviderAttemptOperationPhase, ProviderErrorStage,
};

/// event sink 拒绝事件时使用的不透明错误；原始 sink 错误不会进入 Agent 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLoopEventSinkError;

/// 调用方在单次 AgentLoop 调用期间消费事件的窄 callback。
pub type AgentLoopEventCallback<'a> =
    dyn FnMut(AgentLoopEvent) -> Result<(), AgentLoopEventSinkError> + 'a;

/// AgentLoop 对调用方公开的有序运行时事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentLoopEvent {
    /// 只在 tool-free finalization 请求期间投影的 assistant 文本增量。
    FinalTextDelta { delta: String },
    /// 不含用户或 provider 原文的安全运行时 observation。
    Observation(AgentObservation),
}

/// AgentLoop 当前支持的安全 observation 类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "occurrence", rename_all = "snake_case")]
pub enum AgentObservation {
    PromptAssembly(PromptAssemblyObservation),
    ProviderAttempt(Box<ProviderAttemptObservation>),
    ToolCall(ToolCallObservation),
    PolicyDecision(PolicyDecisionObservation),
    SandboxExecution(SandboxExecutionOccurrence),
    Verification(VerificationObservation),
    VerificationPlan(VerificationPlanObservation),
    RepairPlanning(RepairPlanningObservation),
    FinalReview(FinalReviewObservation),
}

/// 一个 occurrence 在当前 turn 内的稳定身份与父子关联。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OccurrenceIdentity {
    pub occurrence_id: String,
    pub parent_occurrence_id: Option<String>,
    pub ordinal: u32,
}

/// occurrence 的开始、暂停或完成边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum OccurrenceLifecycle<S> {
    Started {
        queued_at_unix_ms: u64,
        started_at_unix_ms: u64,
    },
    Suspended {
        queued_at_unix_ms: u64,
        started_at_unix_ms: u64,
        suspended_at_unix_ms: u64,
        duration_ms: u64,
        status: S,
    },
    Finished {
        queued_at_unix_ms: u64,
        started_at_unix_ms: u64,
        ended_at_unix_ms: u64,
        duration_ms: u64,
        status: S,
    },
}

/// model request 本地装配边界的稳定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptAssemblyStatus {
    Ready,
    ToolViewRejected,
    ContextOverflow,
    ValidationFailed,
}

/// 一次真实 model request 装配与本地校验 occurrence。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptAssemblyObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<PromptAssemblyStatus>,
    pub model_turn_ordinal: u32,
    pub message_count: u32,
    pub tool_count: u32,
    pub request_token_count: u32,
    pub request_digest: String,
    pub compacted: bool,
    pub finalization_only: bool,
}

/// model 返回的一次 tool occurrence 的稳定终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
    PolicyDenied,
    ApprovalRequired,
    BatchRejected,
}

/// 一个真实 model tool occurrence；ordinal 保证重复 tool-call ID 不会碰撞。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<ToolCallStatus>,
    pub model_turn_ordinal: u32,
    pub tool_call_ordinal: u32,
    pub tool_call_id_digest: String,
    pub tool_name: String,
}

/// 最终 policy 决策；不包含 resource、reason、rule ID 或原始参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionStatus {
    Allow,
    Ask,
    Deny,
}

/// policy 决策的稳定、安全因果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionCause {
    Explicit,
    Rule,
    FilesystemProfile,
    NetworkProfile,
    ProtectedResource,
    NoMatchingRule,
    ApprovalPolicy,
    ApprovalGrant,
    ApprovalState,
}

/// 围绕一次真实 `tool_decision` 的 typed occurrence。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PolicyDecisionObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<PolicyDecisionStatus>,
    pub operation_count: u32,
    pub resource_count: u32,
    pub cause: Option<PolicyDecisionCause>,
}

/// 实际进入 `SandboxBackend` 的 command occurrence 终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxExecutionStatus {
    Ok,
    Error,
    TimedOut,
    Cancelled,
}

/// command backend observation 的 Agent 关联投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxExecutionOccurrence {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<SandboxExecutionStatus>,
    pub command_id: String,
    pub command_id_binding_valid: Option<bool>,
    pub workspace_mutation: Option<singularity_tools::WorkspaceMutation>,
    pub enforcement: Option<singularity_tools::SandboxBackendEnforcement>,
}

/// CompletionTracker 的真实 command 观察或 completion gate 结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    CommandPassed,
    CommandFailed,
    GatePassed,
    GateRejected,
    RepairRequested,
}

/// Verification plan lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPlanStatus {
    Planned,
    Rejected,
    Cancelled,
}

/// Safe projection of a revision-bound verification plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationPlanObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<VerificationPlanStatus>,
    pub revision: Option<singularity_tools::WorkspaceRevision>,
    pub risk_count: u32,
    pub requirement_count: u32,
    pub satisfied_requirement_count: u32,
}

/// Repair planning lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepairPlanningStatus {
    Planned,
    Exhausted,
    Cancelled,
}

/// Safe projection of bounded repair planning.  It intentionally carries no raw error, prompt,
/// arguments, path, or audit metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepairPlanningObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<RepairPlanningStatus>,
    pub reason: crate::AgentRepairReason,
    pub attempt: u32,
    pub max_attempts: u32,
    pub required_revision: Option<singularity_tools::WorkspaceRevision>,
}

/// verification occurrence 的安全计数与真实 command duration 关联。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<VerificationStatus>,
    pub required_command_count: u32,
    pub satisfied_command_count: u32,
    pub occurrence_count: u32,
    pub command_duration_ms: Option<u64>,
}

/// provider transport attempt 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Ok,
    Error,
    Cancelled,
}

/// 一次真实 provider transport attempt 的 typed 生命周期 occurrence。
///
/// Start 在 provider 调用之前投影到 SQLite，End 在调用返回后投影；
/// 同一 span ID 关联 Start/End，retry 各自拥有独立 occurrence。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAttemptObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<ProviderAttemptStatus>,
    pub operation_phase: ProviderAttemptOperationPhase,
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    pub attempt_index: u32,
    pub retry_count: u32,
    pub request_send_to_headers_ms: Option<u64>,
    pub time_to_first_text_delta_ms: Option<u64>,
    pub retry_backoff_ms: Option<u64>,
    pub error_category: Option<ModelErrorCategory>,
    pub error_stage: Option<ProviderErrorStage>,
    pub diagnostic_code: Option<String>,
    pub usage: Option<ProviderAttemptUsageObservation>,
}

/// Provider attempt usage fields safe for typed trace projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAttemptUsageObservation {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

/// tool-free finalization-only model request 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinalReviewStatus {
    Succeeded,
    Failed,
    Cancelled,
}

/// 只围绕 `finalization_ready()` 后请求产生的 FinalReview occurrence。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewObservation {
    pub identity: OccurrenceIdentity,
    pub lifecycle: OccurrenceLifecycle<FinalReviewStatus>,
    pub model_turn_ordinal: u32,
    /// Set only on the terminal lifecycle event; `None` on the start event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<crate::FinalReviewVerdict>,
}

/// 单个 occurrence 的真实单调计时器；wall-clock 字段只用于跨层 trace 关联。
pub(crate) struct OccurrenceTimer {
    queued_at_unix_ms: u64,
    started_at_unix_ms: u64,
    started: Instant,
}

impl OccurrenceTimer {
    pub(crate) fn start() -> Self {
        let now = unix_timestamp_ms();
        Self {
            queued_at_unix_ms: now,
            started_at_unix_ms: now,
            started: Instant::now(),
        }
    }

    pub(crate) fn started<S>(&self) -> OccurrenceLifecycle<S> {
        OccurrenceLifecycle::Started {
            queued_at_unix_ms: self.queued_at_unix_ms,
            started_at_unix_ms: self.started_at_unix_ms,
        }
    }

    pub(crate) fn finished<S>(&self, status: S) -> OccurrenceLifecycle<S> {
        OccurrenceLifecycle::Finished {
            queued_at_unix_ms: self.queued_at_unix_ms,
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms: unix_timestamp_ms(),
            duration_ms: elapsed_millis(self.started.elapsed()),
            status,
        }
    }

    pub(crate) fn finished_with_duration<S>(
        &self,
        duration_ms: u64,
        status: S,
    ) -> OccurrenceLifecycle<S> {
        OccurrenceLifecycle::Finished {
            queued_at_unix_ms: self.queued_at_unix_ms,
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms: unix_timestamp_ms(),
            duration_ms,
            status,
        }
    }

    pub(crate) fn suspended<S>(&self, status: S) -> OccurrenceLifecycle<S> {
        OccurrenceLifecycle::Suspended {
            queued_at_unix_ms: self.queued_at_unix_ms,
            started_at_unix_ms: self.started_at_unix_ms,
            suspended_at_unix_ms: unix_timestamp_ms(),
            duration_ms: elapsed_millis(self.started.elapsed()),
            status,
        }
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        elapsed_millis(self.started.elapsed())
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn elapsed_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
