#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, contains_sensitive_text};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelError, ModelErrorCategory,
    ModelMessage, ModelPreferences, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelToolSchema, ModelTurnRequest, ModelTurnStatus, Provider, ProviderDiagnostic,
    ProviderProtocolContract, provider_error_response, validate_model_request_with_capabilities,
    validate_model_turn_response,
};
use singularity_policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, NetworkAccess, PermissionDecision,
    PermissionDecisionOutcome, PermissionOperation, PermissionProfile, PermissionProfileName,
    PermissionRequest, PolicyEngine,
};
use singularity_tools::{
    CommandToolInput, EditToolInput, GrepToolInput, ListToolInput, ReadToolInput,
    SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolBrokerDecision, ToolCallRequest,
    ToolOutput, ToolResult, WorkspacePatch, WorkspaceToolError, WorkspaceTools,
    command_scope_digest, command_scope_resource, is_protected_path,
};
use thiserror::Error;

#[cfg(not(windows))]
const STRICT_COMMAND_SANDBOX_UNSUPPORTED_PLATFORM: &str =
    "strict_command_sandbox_unsupported_platform";
#[cfg(windows)]
const AGENT_LOOP_READY_REASON: &str = "AgentLoop uses automatic Windows elevated sandbox setup with restricted-token fallback only for network-enabled profiles";
#[cfg(not(windows))]
const AGENT_LOOP_UNSUPPORTED_PLATFORM_REASON: &str =
    "AgentLoop requires the Windows restricted-token command sandbox";
const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 16;
const MAX_TOOL_CALLS_PER_TURN: u32 = 1;
const APPROVAL_CHECKPOINT_VERSION: u32 = 1;
const AGENT_DEVELOPER_INSTRUCTIONS: &str = "You are a coding agent working in the current workspace. Use the available tools to inspect real files before making claims. Issue at most one tool call in each assistant response, then wait for its result before continuing. Make requested changes through tools, keep all writes inside the workspace, and run a relevant verification command after the last workspace mutation. Report only work and verification that actually completed. Read-only questions may be answered without changing files or running verification.";
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
const COMMAND_SANDBOX_PROFILE_DENIED: &str =
    "command sandbox mode cannot exceed the permission profile";
const COMMAND_NETWORK_PROFILE_DENIED: &str =
    "command network access cannot exceed the permission profile";
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
const TOOL_READ: &str = "builtin.read";
const TOOL_LIST: &str = "builtin.list";
const TOOL_GREP: &str = "builtin.grep";
const TOOL_EDIT: &str = "builtin.edit";
const TOOL_PATCH: &str = "builtin.patch";
const TOOL_COMMAND: &str = "builtin.command";
const TOOL_UPDATE_PLAN: &str = "builtin.update_plan";
const REPEATED_FAILURE_RECOVERY_INSTRUCTIONS: &str = "The same repairable tool failure recurred. Read the registered tool schema and the previous tool result, then choose a different next action. Do not repeat the same call.";
const PLAN_COMPLETION_REQUIRED: &str = "Do not finalize yet. Complete every plan step, then call builtin.update_plan with all steps marked completed before providing the final answer.";

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct AgentVerification {
    pub required: bool,
    pub passed: bool,
    pub successful_command_count: u32,
    pub unresolved_failures: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanStep {
    pub step: String,
    pub status: AgentPlanStepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlan {
    pub steps: Vec<AgentPlanStep>,
}

impl AgentPlan {
    pub fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("plan must contain at least one step".to_string());
        }
        let mut unique_steps = BTreeSet::new();
        let mut in_progress_count = 0usize;
        for plan_step in &self.steps {
            let normalized_step = plan_step.step.trim();
            if normalized_step.is_empty() {
                return Err("plan steps must not be empty".to_string());
            }
            if !unique_steps.insert(normalized_step.to_string()) {
                return Err("plan steps must be unique".to_string());
            }
            if plan_step.status == AgentPlanStepStatus::InProgress {
                in_progress_count += 1;
            }
        }
        if in_progress_count > 1 {
            return Err("plan may have at most one in_progress step".to_string());
        }
        Ok(())
    }

    pub fn is_completed(&self) -> bool {
        self.steps
            .iter()
            .all(|plan_step| plan_step.status == AgentPlanStepStatus::Completed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanUpdateInput {
    pub steps: Vec<AgentPlanStep>,
}

impl AgentPlanUpdateInput {
    pub fn into_plan(self) -> Result<AgentPlan, String> {
        let plan = AgentPlan { steps: self.steps };
        plan.validate()?;
        Ok(plan)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRecoveryMetrics {
    pub invalid_tool_call_count: u32,
    pub repeated_tool_call_count: u32,
    pub repair_attempt_count: u32,
    pub completion_rejection_count: u32,
}

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
}

impl AgentRunStatus {
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
            error: Some(message.into()),
            model_turn_limit: 0,
            context_trace: None,
            error_category: None,
            provider_diagnostic: None,
        }
    }

    pub fn failed_with_category(
        message: impl Into<String>,
        error_category: Option<ModelErrorCategory>,
    ) -> Self {
        let mut status = Self::failed(message);
        status.error_category = error_category;
        status
    }

    pub fn with_status(mut self, status: AgentStatus) -> Self {
        self.completed = status == AgentStatus::Completed;
        self.status = status;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopCapability {
    pub available: bool,
    pub status: AgentStatus,
    pub reason: String,
    pub blockers: Vec<String>,
}

impl AgentLoopCapability {
    pub fn current() -> Self {
        #[cfg(windows)]
        {
            Self {
                available: true,
                status: AgentStatus::Completed,
                reason: AGENT_LOOP_READY_REASON.to_string(),
                blockers: Vec::new(),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                available: false,
                status: AgentStatus::Blocked,
                reason: AGENT_LOOP_UNSUPPORTED_PLATFORM_REASON.to_string(),
                blockers: vec![STRICT_COMMAND_SANDBOX_UNSUPPORTED_PLATFORM.to_string()],
            }
        }
    }
}

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
}

impl AgentLoopInput {
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
        }
    }

    pub fn with_model_name(mut self, model_name: Option<String>) -> Self {
        self.model_preferences.model_name = model_name;
        self
    }

    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    pub fn with_project_instructions(mut self, instructions: impl Into<String>) -> Self {
        let instructions = instructions.into();
        self.project_instructions = (!instructions.trim().is_empty()).then_some(instructions);
        self
    }

    pub fn with_history(mut self, history: impl IntoIterator<Item = AgentContextItem>) -> Self {
        let mut history: Vec<AgentContextItem> = history
            .into_iter()
            .filter_map(AgentContextItem::into_safe_history)
            .collect();
        history.extend(self.input);
        self.input = history;
        self
    }

    pub fn with_approval_grant(mut self, grant: ApprovalGrant) -> Self {
        self.approval_grants.push(grant);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalGrant {
    pub request_id: String,
    pub tool_name: String,
    pub resources: Vec<String>,
    pub outcome: ApprovalOutcome,
}

impl ApprovalGrant {
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
}

impl AgentLoopResult {
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
            error: self.error.clone(),
            model_turn_limit: self.model_turn_limit,
            context_trace: self.context_trace.clone(),
            error_category: self.error_category.clone(),
            provider_diagnostic: self.provider_diagnostic.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PendingToolCall {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
    pub resources: Vec<String>,
}

impl PendingToolCall {
    pub fn new(input: &AgentLoopInput, call: &ModelToolCall) -> Self {
        Self {
            request_id: approval_request_id(input, call),
            tool_call_id: call.tool_call_id.clone(),
            tool_name: call.tool_name.clone(),
            raw_arguments: call.raw_arguments.clone(),
            resources: permission_resources_for_tool(call),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct CompletionTracker {
    workspace_mutated: bool,
    verified_after_last_mutation: bool,
    successful_command_count: u32,
    unresolved_failures: BTreeSet<String>,
}

impl CompletionTracker {
    fn observe(&mut self, tool_result: &ToolResult) {
        let failure_group = match tool_result.tool_name.as_str() {
            TOOL_EDIT | TOOL_PATCH => "workspace_mutation",
            TOOL_COMMAND => "verification",
            tool_name => tool_name,
        };
        if tool_result.ok {
            self.unresolved_failures
                .retain(|failure| !failure.starts_with(failure_group));
            if matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH) {
                self.workspace_mutated = true;
                self.verified_after_last_mutation = false;
            } else if tool_result.tool_name == TOOL_COMMAND {
                self.successful_command_count = self.successful_command_count.saturating_add(1);
                if self.workspace_mutated {
                    self.verified_after_last_mutation = true;
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
        self.unresolved_failures.is_empty()
            && (!self.workspace_mutated || self.verified_after_last_mutation)
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
        "Do not finalize yet. Run a relevant verification command after the latest workspace mutation, inspect its result, and only then provide the final answer."
            .to_string()
    }

    fn summary(&self) -> AgentVerification {
        AgentVerification {
            required: self.workspace_mutated,
            passed: self.workspace_mutated
                && self.verified_after_last_mutation
                && self.unresolved_failures.is_empty(),
            successful_command_count: self.successful_command_count,
            unresolved_failures: self.unresolved_failures.iter().cloned().collect(),
        }
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
    audit_metadata: Option<Value>,
}

impl CheckpointToolResult {
    fn from_tool_result(result: &ToolResult) -> Self {
        Self {
            result: result.clone(),
            audit_metadata: result.audit_metadata().cloned(),
        }
    }

    fn into_tool_result(self) -> ToolResult {
        match self.audit_metadata {
            Some(audit_metadata) => self.result.with_audit(audit_metadata),
            None => self.result,
        }
    }
}

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
    seen_tool_call_fingerprints: Vec<String>,
    #[serde(default)]
    last_repair_failure: Option<RepairFailureState>,
}

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
    seen_tool_call_fingerprints: BTreeSet<String>,
    last_repair_failure: Option<RepairFailureState>,
    model_turn_limit: u32,
    context_trace: Option<AgentContextTrace>,
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
            seen_tool_call_fingerprints: BTreeSet::new(),
            last_repair_failure: None,
            model_turn_limit,
            context_trace,
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
            plan: self.plan,
            plan_update_count: self.plan_update_count,
            recovery_metrics: self.recovery_metrics,
            error,
            model_turn_limit: self.model_turn_limit,
            context_trace: self.context_trace,
            error_category: model_error.map(ModelError::category),
            provider_diagnostic: model_error.map(ModelError::provider_diagnostic),
        }
    }

    fn approval_count(&self) -> u32 {
        self.prior_approval_count
            .saturating_add(self.approval_requests.len() as u32)
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
            seen_tool_call_fingerprints: self.seen_tool_call_fingerprints.iter().cloned().collect(),
            last_repair_failure: self.last_repair_failure.clone(),
        };
        serde_json::to_value(checkpoint).expect("AgentLoop checkpoint serializes")
    }

    fn allows_final(&self) -> bool {
        self.completion.allows_final() && self.plan.as_ref().is_none_or(AgentPlan::is_completed)
    }

    fn completion_rejection_reason(&self) -> String {
        if !self.completion.allows_final() {
            return self.completion.rejection_reason();
        }
        "completion gate rejected final answer: plan has incomplete steps".to_string()
    }

    fn completion_feedback(&self) -> String {
        if !self.completion.allows_final() {
            return self.completion.feedback();
        }
        PLAN_COMPLETION_REQUIRED.to_string()
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
    pub fn new(provider: P, tool_broker: ToolBroker, policy: PolicyEngine) -> Self {
        Self {
            provider,
            tool_broker,
            policy,
            workspace_tools: None,
            cancellation: CancellationToken::new(),
        }
    }

    pub fn with_workspace_tools(mut self, workspace_tools: WorkspaceTools) -> Self {
        self.workspace_tools = Some(workspace_tools);
        self
    }

    pub fn with_cancellation_token(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn run(&self, input: &AgentLoopInput) -> AgentLoopResult {
        let capabilities = self.provider.protocol_contract();
        let budget = match context_budget(input, &self.tool_broker, &capabilities) {
            Ok(budget) => budget,
            Err(error) => return failed_result(error),
        };
        let context = assemble_context_items_with_budget(&input.input, &budget);
        if current_turn_excluded(input, &context) {
            return context_overflow_result();
        }
        let state = AgentLoopState::new(
            model_messages_from_input(input, &context),
            input.max_turns.max(1),
            Some(AgentContextTrace::from(&context)),
        );
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, 0, None);
        }
        self.continue_run(input, &budget, &capabilities, state, 0)
    }

    fn continue_run(
        &self,
        input: &AgentLoopInput,
        budget: &ContextBudget,
        capabilities: &ProviderProtocolContract,
        mut state: AgentLoopState,
        model_turn_offset: u32,
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
        for turn_index in model_turn_offset..max_turns {
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index, None);
            }
            if !model_request_fits_context(&self.tool_broker, &state.messages, budget) {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index,
                    Some(MODEL_REQUEST_CONTEXT_OVERFLOW_ERROR.to_string()),
                );
            }
            let request = model_turn_request(
                &self.tool_broker,
                input,
                budget,
                turn_index,
                state.messages.clone(),
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
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
            }
            if response.status != ModelTurnStatus::Success {
                let model_error = response.error.as_ref();
                return state.finish_with_model_error(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index + 1,
                    model_error.map(|error| error.message.clone()),
                    model_error,
                );
            }
            let allowed_tool_names = model_tool_names(&self.tool_broker);
            let observed_tool_calls = response
                .tool_calls
                .iter()
                .map(|call| state.observe_model_tool_call(call, &allowed_tool_names))
                .collect::<Vec<_>>();
            let validation = validate_model_turn_response(
                &request,
                &response,
                &allowed_tool_names,
                Some(capabilities),
            );
            if !validation.valid {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index + 1,
                    Some(format!(
                        "{MODEL_RESPONSE_VALIDATION_ERROR}: {}",
                        validation.errors.join(", ")
                    )),
                );
            }
            if response.assistant_message.as_ref().is_some_and(|message| {
                !message.tool_calls.is_empty() && message.tool_calls != response.tool_calls
            }) {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index + 1,
                    Some(format!(
                        "{MODEL_RESPONSE_VALIDATION_ERROR}: assistant_tool_calls_mismatch"
                    )),
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
                        turn_index + 1,
                        Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    );
                }
                if state.allows_final() {
                    return state.finish(
                        AgentStatus::Completed,
                        true,
                        Some(final_answer),
                        turn_index + 1,
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
            let mut assistant_tool_message = response
                .assistant_message
                .clone()
                .unwrap_or_else(|| ModelMessage::assistant_tool_calls(response.tool_calls.clone()));
            if assistant_tool_message.tool_calls.is_empty() {
                assistant_tool_message.tool_calls = response.tool_calls.clone();
            }
            state.messages.push(assistant_tool_message);
            for (provider_call, (tool_call_fingerprint, invalid_was_observed)) in
                response.tool_calls.iter().zip(observed_tool_calls.iter())
            {
                let (bound_call, argument_error, forced_decision) =
                    match validate_tool_call_arguments(provider_call) {
                        Err(error) => (provider_call.clone(), Some(error), None),
                        Ok(()) => {
                            match bind_tool_call_to_profile(provider_call, &self.policy.profile) {
                                Ok(call) => (call, None, None),
                                Err(CommandBindingError::InvalidArguments(error)) => {
                                    (provider_call.clone(), Some(error), None)
                                }
                                Err(CommandBindingError::ProfileViolation(reason)) => (
                                    provider_call.clone(),
                                    None,
                                    Some(ToolBrokerDecision::deny(reason)),
                                ),
                            }
                        }
                    };
                let call = &bound_call;
                if self.is_cancelled(input) {
                    return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
                }
                let tool_result = if let Some(error) = argument_error {
                    if !*invalid_was_observed {
                        state.recovery_metrics.invalid_tool_call_count = state
                            .recovery_metrics
                            .invalid_tool_call_count
                            .saturating_add(1);
                    }
                    invalid_tool_arguments_result(
                        call,
                        error,
                        self.tool_broker
                            .get(&call.tool_name)
                            .map(|spec| &spec.input_schema),
                    )
                } else {
                    let decision = forced_decision.unwrap_or_else(|| {
                        self.tool_decision(input, call, &mut state.used_approval_grants)
                    });
                    if let ToolBrokerDecision::Ask {
                        approval_request_id,
                        reason,
                    } = &decision
                    {
                        state.approval_requests.push(approval_request(
                            input,
                            approval_request_id,
                            call,
                            reason,
                        ));
                        state
                            .pending_tool_calls
                            .push(PendingToolCall::new(input, call));
                        let pending = state
                            .pending_tool_calls
                            .last()
                            .expect("pending tool call was just inserted")
                            .clone();
                        let checkpoint = state.checkpoint(input, &pending, turn_index + 1);
                        state.approval_checkpoints.push(checkpoint);
                        let tool_result = self.execute_tool(call, decision, &mut state);
                        state.tool_results.push(tool_result);
                        return state.finish(
                            AgentStatus::Blocked,
                            false,
                            None,
                            turn_index + 1,
                            None,
                        );
                    }
                    self.execute_tool(call, decision, &mut state)
                };
                let failed_tool_result = !tool_result.ok;
                let recovery_feedback =
                    state.observe_tool_result(&tool_result, tool_call_fingerprint);
                state.tool_results.push(tool_result.clone());
                state.messages.push(tool_result_message(&tool_result));
                if let Some(feedback) = recovery_feedback {
                    state
                        .messages
                        .push(ModelMessage::text(ModelRole::Developer, feedback));
                }
                if self.is_cancelled(input) {
                    return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
                }
                if failed_tool_result {
                    if is_repairable_tool_result(&tool_result) {
                        continue;
                    }
                    let error_code = tool_result
                        .error_code
                        .as_deref()
                        .unwrap_or("tool_execution_failed");
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        turn_index + 1,
                        Some(format!("tool execution failed: {error_code}")),
                    );
                }
            }
        }
        let error = state
            .last_completion_error
            .take()
            .unwrap_or_else(|| "max turns exceeded".to_string());
        state.finish(AgentStatus::Failed, false, None, max_turns, Some(error))
    }

    pub fn resume_pending_tool_call(
        &self,
        input: &AgentLoopInput,
        pending: &PendingToolCall,
        checkpoint_payload: &Value,
    ) -> AgentLoopResult {
        let call = match pending
            .to_model_tool_call()
            .map_err(|error| format!("invalid pending tool call arguments: {error}"))
            .and_then(|call| {
                bind_tool_call_to_profile(&call, &self.policy.profile)
                    .map_err(|error| error.to_string())
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
        let (mut state, model_turn_offset) =
            match restore_checkpoint(input, pending, checkpoint_payload) {
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
        let capabilities = self.provider.protocol_contract();
        let budget = match context_budget(input, &self.tool_broker, &capabilities) {
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
        let context = assemble_context_items_with_budget(&input.input, &budget);
        state.context_trace = Some(AgentContextTrace::from(&context));
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
        let failed_tool_result = !tool_result.ok;
        let recovery_feedback = state.observe_tool_result(&tool_result, &tool_call_fingerprint);
        state.messages.push(tool_result_message(&tool_result));
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
        self.continue_run(input, &budget, &capabilities, state, model_turn_offset)
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
            return ToolBrokerDecision::deny("invalid tool call arguments");
        }
        let request_id = approval_request_id(input, call);
        let resources = permission_resources_for_tool(call);
        let permission = self.tool_permission_decision(call);
        if used_approval_grants.contains(&request_id) {
            return ToolBrokerDecision::deny("approval grant already consumed");
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
            PermissionDecisionOutcome::Deny => ToolBrokerDecision::deny(permission.reason),
            PermissionDecisionOutcome::Ask => {
                ToolBrokerDecision::ask(request_id, permission.reason)
            }
        }
    }

    fn tool_permission_decision(&self, call: &ModelToolCall) -> PermissionDecision {
        let resources = permission_resources_for_tool(call);
        let mut operations = vec![permission_operation_for_tool(&call.tool_name)];
        if call.tool_name == TOOL_COMMAND
            && command_tool_input(&call.arguments)
                .is_ok_and(|input| input.network_access() != SandboxNetworkMode::Denied)
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

    fn execute_tool(
        &self,
        call: &ModelToolCall,
        decision: ToolBrokerDecision,
        state: &mut AgentLoopState,
    ) -> ToolResult {
        let envelope = ToolCallRequest::new(
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            call.raw_arguments.clone(),
        );
        let executor_decision = decision.clone();
        let mut result = self
            .tool_broker
            .execute(&envelope, decision.clone(), |_envelope| {
                if call.tool_name == TOOL_UPDATE_PLAN {
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
        if self.cancellation.is_cancelled() {
            return ToolOutput::failure(
                "tool_cancelled",
                json!({"summary": "tool execution cancelled"}),
            );
        }
        let Some(workspace_tools) = &self.workspace_tools else {
            return ToolOutput::failure(
                "backend_unavailable",
                json!({"summary": "workspace tool backend is unavailable"}),
            );
        };
        let result = match call.tool_name.as_str() {
            TOOL_READ => read_tool_input(&call.arguments)
                .and_then(|input| workspace_tools.read(input).map_err(Into::into)),
            TOOL_LIST => list_tool_input(&call.arguments)
                .and_then(|input| workspace_tools.list(input).map_err(Into::into)),
            TOOL_GREP => grep_tool_input(&call.arguments)
                .and_then(|input| workspace_tools.grep(input).map_err(Into::into)),
            TOOL_EDIT => edit_tool_input(&call.arguments)
                .and_then(|input| workspace_tools.edit(input, decision).map_err(Into::into)),
            TOOL_PATCH => patch_tool_input(&call.arguments)
                .and_then(|input| workspace_tools.patch(input, decision).map_err(Into::into)),
            TOOL_COMMAND => match command_tool_input(&call.arguments) {
                Ok(input) => Ok(workspace_tools
                    .command_cancellable(input.clone(), &self.cancellation)
                    .map_err(Into::into)
                    .unwrap_or_else(|error| command_workspace_tool_failure(&input, error))),
                Err(error) => Err(error),
            },
            _ => Ok(ToolOutput::failure(
                "backend_unavailable",
                json!({"summary": "tool backend is unavailable"}),
            )),
        };
        result.unwrap_or_else(workspace_tool_failure)
    }
}

#[derive(Debug, Error)]
enum AgentLoopToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("{0}")]
    Workspace(#[from] WorkspaceToolError),
}

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

    pub fn history_user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::history(item_id, content, USER_MESSAGE_ROLE)
    }

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

fn context_budget(
    input: &AgentLoopInput,
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
) -> Result<ContextBudget, String> {
    if capabilities.max_context_tokens == 0 || capabilities.max_output_tokens == 0 {
        return Err("provider token capabilities must be greater than zero".to_string());
    }
    let developer_instruction_tokens = approximate_token_count(&developer_instructions(input));
    let tool_tokens = serde_json::to_string(&model_tool_schemas(loop_tools))
        .map_or(u32::MAX, |tools| approximate_token_count(&tools));
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
    loop_tools: &ToolBroker,
    messages: &[ModelMessage],
    budget: &ContextBudget,
) -> bool {
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
                "name": message.name,
                "tool_call_id": message.tool_call_id,
                "tool_calls": tool_calls,
            })
        })
        .collect::<Vec<_>>();
    let projected_tools = model_tool_schemas(loop_tools)
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters_schema,
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
        <= budget.model_context_window
}

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

fn restore_checkpoint(
    input: &AgentLoopInput,
    pending: &PendingToolCall,
    payload: &Value,
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
        || last_message.tool_calls.len() != MAX_TOOL_CALLS_PER_TURN as usize
        || last_message.tool_calls[0].tool_call_id != pending.tool_call_id
    {
        return Err("approval checkpoint assistant tool-call ordering is invalid".to_string());
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
    let tool_results = checkpoint
        .tool_results
        .into_iter()
        .map(CheckpointToolResult::into_tool_result)
        .collect::<Vec<_>>();
    let mut derived_completion = CompletionTracker::default();
    for tool_result in &tool_results {
        derived_completion.observe(tool_result);
    }
    if derived_completion != checkpoint.completion {
        return Err("approval checkpoint completion state mismatch".to_string());
    }
    let derived_plan_update_count = tool_results
        .iter()
        .filter(|tool_result| tool_result.tool_name == TOOL_UPDATE_PLAN && tool_result.ok)
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
    state.seen_tool_call_fingerprints = seen_tool_call_fingerprints;
    state.last_repair_failure = checkpoint.last_repair_failure;
    Ok((state, checkpoint.model_turns))
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
        error: Some(error.into()),
        model_turn_limit: 0,
        context_trace: None,
        error_category: None,
        provider_diagnostic: None,
    }
}

fn context_overflow_result() -> AgentLoopResult {
    failed_result(CURRENT_TURN_CONTEXT_OVERFLOW_ERROR)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundle {
    pub messages: Vec<Value>,
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentContextTrace {
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
}

impl From<&ContextBundle> for AgentContextTrace {
    fn from(context: &ContextBundle) -> Self {
        Self {
            included_item_ids: context.included_item_ids.clone(),
            excluded_item_ids: context.excluded_item_ids.clone(),
            budget: context.budget.clone(),
        }
    }
}

fn model_turn_request(
    loop_tools: &ToolBroker,
    input: &AgentLoopInput,
    budget: &ContextBudget,
    turn_index: u32,
    messages: Vec<ModelMessage>,
) -> ModelTurnRequest {
    let mut request = ModelTurnRequest {
        request_id: format!("model_request_{}_{}", input.turn_id, turn_index),
        messages,
        tools: model_tool_schemas(loop_tools),
        tool_choice: Default::default(),
        model_preferences: ModelPreferences {
            max_output_tokens: Some(budget.reserved_output_tokens),
            ..input.model_preferences.clone()
        },
    };
    request.tool_choice.max_tool_calls = MAX_TOOL_CALLS_PER_TURN;
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

fn model_tool_names(loop_tools: &ToolBroker) -> Vec<String> {
    model_tool_schemas(loop_tools)
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn model_messages_from_input(input: &AgentLoopInput, context: &ContextBundle) -> Vec<ModelMessage> {
    let mut messages = vec![ModelMessage::text(
        ModelRole::Developer,
        developer_instructions(input),
    )];
    messages.extend(model_messages_from_context(context));
    messages
}

fn developer_instructions(input: &AgentLoopInput) -> String {
    match input.project_instructions.as_deref() {
        Some(project) => {
            format!("{AGENT_DEVELOPER_INSTRUCTIONS}\n\nProject instructions:\n{project}")
        }
        None => AGENT_DEVELOPER_INSTRUCTIONS.to_string(),
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

fn tool_result_message(tool_result: &ToolResult) -> ModelMessage {
    let mut message = ModelMessage::text(
        ModelRole::Tool,
        tool_result.to_message_payload().to_string(),
    );
    message.tool_call_id = Some(tool_result.tool_call_id.clone());
    message.name = Some(tool_result.tool_name.clone());
    message
}

fn audit_events_from_tool_results(tool_results: &[ToolResult]) -> Vec<Value> {
    tool_results
        .iter()
        .filter_map(|result| result.audit_metadata().cloned())
        .collect()
}

fn command_audit_metadata(
    existing: Option<&Value>,
    call: &ModelToolCall,
    decision: &ToolBrokerDecision,
    approval_policy: ApprovalPolicy,
) -> Value {
    let mut audit = existing.cloned().unwrap_or_else(|| json!({}));
    if let Ok(input) = command_tool_input(&call.arguments) {
        if audit.get("cwd").is_none() {
            audit["cwd"] = json!(input.effective_cwd());
        }
        if audit.get("timeout_seconds").is_none() {
            audit["timeout_seconds"] = json!(input.effective_timeout_seconds());
        }
        if audit.get("sandbox_mode").is_none() {
            audit["sandbox_mode"] =
                serde_json::to_value(input.sandbox_mode()).unwrap_or(json!("unknown"));
        }
        if audit.get("network_access").is_none() {
            audit["network_access"] =
                serde_json::to_value(input.network_access()).unwrap_or(json!("unknown"));
        }
        if audit.get("command_scope_digest").is_none() {
            audit["command_scope_digest"] = json!(command_scope_digest(
                &input.argv,
                input.effective_cwd(),
                input.effective_timeout_seconds(),
                &input.sandbox_mode(),
                &input.network_access(),
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
            ToolBrokerDecision::Deny { reason } => {
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
    tool_result
        .error_code
        .as_deref()
        .is_some_and(|error_code| REPAIRABLE_TOOL_ERROR_CODES.contains(&error_code))
}

fn safe_plan_summary(plan: &AgentPlan) -> Value {
    json!({
        "steps": plan
            .steps
            .iter()
            .map(|plan_step| json!({
                "step": safe_plan_step_text(&plan_step.step),
                "status": plan_step.status,
            }))
            .collect::<Vec<_>>(),
    })
}

fn safe_plan_step_text(step: &str) -> String {
    if contains_sensitive_text(step) {
        return "[redacted plan step]".to_string();
    }
    step.chars().take(512).collect()
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
) -> ApprovalRequest {
    let mut request = ApprovalRequest::new(
        approval_request_id,
        input.thread_id.clone(),
        input.turn_id.clone(),
        call.tool_name.clone(),
    )
    .with_tool_call_id(call.tool_call_id.clone())
    .with_resources(permission_resources_for_tool(call));
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
    #[error("{0}")]
    ProfileViolation(String),
}

fn bind_tool_call_to_profile(
    call: &ModelToolCall,
    profile: &PermissionProfile,
) -> Result<ModelToolCall, CommandBindingError> {
    if call.tool_name != TOOL_COMMAND {
        return Ok(call.clone());
    }
    let mut input =
        command_tool_input(&call.arguments).map_err(CommandBindingError::InvalidArguments)?;
    let cwd = input.effective_cwd().to_string();
    let timeout_seconds = input.effective_timeout_seconds();
    let (sandbox_mode, network_access) =
        effective_command_policy(profile, &input).map_err(CommandBindingError::ProfileViolation)?;
    input.cwd = Some(cwd);
    input.timeout_seconds = Some(timeout_seconds);
    input.sandbox_mode = Some(sandbox_mode);
    input.network_access = Some(network_access);
    let arguments = serde_json::to_value(input)
        .map_err(|error| CommandBindingError::ProfileViolation(error.to_string()))?;
    let mut bound = call.clone();
    bound.raw_arguments = arguments.to_string();
    bound.arguments = arguments;
    Ok(bound)
}

fn effective_command_policy(
    profile: &PermissionProfile,
    input: &CommandToolInput,
) -> Result<(SandboxFilesystemMode, SandboxNetworkMode), String> {
    let session_filesystem = match profile.profile {
        PermissionProfileName::ReadOnly => SandboxFilesystemMode::ReadOnly,
        PermissionProfileName::WorkspaceWrite => SandboxFilesystemMode::WorkspaceWrite,
        PermissionProfileName::DangerFullAccess => SandboxFilesystemMode::DangerFullAccess,
    };
    let requested_filesystem = input
        .sandbox_mode
        .clone()
        .unwrap_or(session_filesystem.clone());
    if !filesystem_request_within_profile(&session_filesystem, &requested_filesystem) {
        return Err(COMMAND_SANDBOX_PROFILE_DENIED.to_string());
    }

    let session_network = match profile.network_access {
        NetworkAccess::Denied => SandboxNetworkMode::Denied,
        NetworkAccess::Allowed => SandboxNetworkMode::Allowed,
    };
    let requested_network = input
        .network_access
        .clone()
        .unwrap_or(session_network.clone());
    if session_network == SandboxNetworkMode::Denied
        && requested_network != SandboxNetworkMode::Denied
    {
        return Err(COMMAND_NETWORK_PROFILE_DENIED.to_string());
    }
    Ok((requested_filesystem, requested_network))
}

fn filesystem_request_within_profile(
    profile: &SandboxFilesystemMode,
    requested: &SandboxFilesystemMode,
) -> bool {
    match profile {
        SandboxFilesystemMode::ReadOnly => requested == &SandboxFilesystemMode::ReadOnly,
        SandboxFilesystemMode::WorkspaceWrite => {
            requested != &SandboxFilesystemMode::DangerFullAccess
        }
        SandboxFilesystemMode::DangerFullAccess => true,
    }
}

fn permission_operation_for_tool(tool_name: &str) -> PermissionOperation {
    match tool_name {
        TOOL_EDIT | TOOL_PATCH => PermissionOperation::Write,
        TOOL_COMMAND => PermissionOperation::Execute,
        _ => PermissionOperation::Read,
    }
}

fn permission_resources_for_tool(call: &ModelToolCall) -> Vec<String> {
    if call.tool_name == TOOL_COMMAND {
        return command_permission_resources(&call.arguments, &call.tool_name);
    }
    let paths = path_arguments(&call.arguments);
    if paths.is_empty() {
        vec![call.tool_name.clone()]
    } else {
        paths
    }
}

fn command_permission_resources(arguments: &Value, fallback: &str) -> Vec<String> {
    let Ok(input) = serde_json::from_value::<CommandToolInput>(arguments.clone()) else {
        return vec![fallback.to_string()];
    };
    let resource = command_scope_resource(
        &input.argv,
        input.effective_cwd(),
        input.effective_timeout_seconds(),
        &input.sandbox_mode(),
        &input.network_access(),
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

fn validate_tool_call_arguments(call: &ModelToolCall) -> Result<(), AgentLoopToolError> {
    match call.tool_name.as_str() {
        TOOL_READ => read_tool_input(&call.arguments).map(|_| ()),
        TOOL_LIST => list_tool_input(&call.arguments).map(|_| ()),
        TOOL_GREP => grep_tool_input(&call.arguments).map(|_| ()),
        TOOL_EDIT => edit_tool_input(&call.arguments).map(|_| ()),
        TOOL_PATCH => patch_tool_input(&call.arguments).map(|_| ()),
        TOOL_COMMAND => command_tool_input(&call.arguments).map(|_| ()),
        TOOL_UPDATE_PLAN => update_plan_tool_input(&call.arguments).map(|_| ()),
        _ => Ok(()),
    }
}

fn invalid_tool_arguments_result(
    call: &ModelToolCall,
    error: AgentLoopToolError,
    expected_schema: Option<&Value>,
) -> ToolResult {
    let envelope = ToolCallRequest::new(
        call.tool_call_id.clone(),
        call.tool_name.clone(),
        call.raw_arguments.clone(),
    );
    let mut audit = json!({
        "argument_validation": "failed",
        "policy_evaluated": false,
        "executor_started": false,
        "tool_provenance": "agent_requested",
    });
    if call.tool_name == TOOL_COMMAND {
        audit["sandbox_backend"] = json!("not_executed");
        audit["command_provenance"] = json!("agent_requested");
        audit["argument_validation_code"] = json!(command_argument_validation_code(call));
    }
    let output = if call.tool_name == TOOL_COMMAND {
        invalid_command_arguments_output(call, error, expected_schema)
    } else {
        workspace_tool_failure(error)
    };
    ToolResult::from_result(&envelope, &output).with_audit(audit)
}

fn command_argument_validation_code(call: &ModelToolCall) -> &'static str {
    match call.arguments.get("argv") {
        None => "missing_argv",
        Some(Value::Array(_)) => "invalid_command_arguments",
        Some(_) => "argv_not_array",
    }
}

fn invalid_command_arguments_output(
    call: &ModelToolCall,
    _error: AgentLoopToolError,
    expected_schema: Option<&Value>,
) -> ToolOutput {
    const MAX_SCHEMA_HINT_CHARS: usize = 2_048;
    let validation_code = command_argument_validation_code(call);
    // Serde's error text can include the offending scalar value. Keep the
    // validation code and schema hints useful to the model, but never echo
    // the raw argument payload through a public tool result.
    let mut summary = format!("invalid command arguments ({validation_code})");
    let retry_inputs = expected_schema
        .map(exact_command_retry_inputs)
        .unwrap_or_default();
    if !retry_inputs.is_empty() {
        summary.push_str(
            ". The argv field must be a JSON array of strings. Copy one complete retry_inputs object exactly",
        );
    }
    if let Some(schema) = expected_schema {
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
    ToolOutput::failure(
        "invalid_tool_arguments",
        json!({
            "summary": summary,
            "validation_code": validation_code,
            "retry_inputs": retry_inputs,
        }),
    )
}

fn exact_command_retry_inputs(schema: &Value) -> Vec<Value> {
    let branches = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(schema));
    branches
        .iter()
        .filter_map(|branch| {
            let properties = branch.get("properties")?.as_object()?;
            let required = branch.get("required")?.as_array()?;
            let mut input = serde_json::Map::new();
            for name in required.iter().filter_map(Value::as_str) {
                let value = properties.get(name)?.get("const")?.clone();
                input.insert(name.to_string(), value);
            }
            Some(Value::Object(input))
        })
        .collect()
}

fn invalid_tool_arguments(error: serde_json::Error) -> AgentLoopToolError {
    AgentLoopToolError::InvalidArguments(error.to_string())
}

fn workspace_tool_failure(error: AgentLoopToolError) -> ToolOutput {
    let error_code = match &error {
        AgentLoopToolError::InvalidArguments(_) => "invalid_tool_arguments",
        AgentLoopToolError::Workspace(WorkspaceToolError::OutsideWorkspace(_)) => {
            "outside_workspace"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ProtectedPath(_)) => "protected_path",
        AgentLoopToolError::Workspace(WorkspaceToolError::SandboxUnavailable) => {
            "sandbox_unavailable"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::BinaryPattern) => "binary_pattern",
        AgentLoopToolError::Workspace(WorkspaceToolError::ReadFailed(_)) => "tool_read_failed",
        AgentLoopToolError::Workspace(WorkspaceToolError::RollbackFailed(_)) => {
            "workspace_rollback_failed"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ExpectedContentMissing(_)) => {
            "expected_content_missing"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::InvalidInput(_)) => "invalid_tool_input",
    };
    let summary = match error {
        // Serde's error text may contain a value copied from the model's raw
        // arguments. The model still receives the stable error code and can
        // consult the registered schema; it must not receive that payload
        // echoed back through the public result.
        AgentLoopToolError::InvalidArguments(_) => "tool arguments failed schema validation",
        _ => return ToolOutput::failure(error_code, json!({"summary": error.to_string()})),
    };
    ToolOutput::failure(error_code, json!({"summary": summary}))
}

fn command_workspace_tool_failure(
    input: &CommandToolInput,
    error: AgentLoopToolError,
) -> ToolOutput {
    let mut output = workspace_tool_failure(error);
    output.metadata["audit"] = json!({
        "cwd": input.effective_cwd(),
        "timeout_seconds": input.effective_timeout_seconds(),
        "sandbox_mode": input.sandbox_mode(),
        "network_access": input.network_access(),
        "sandbox_backend": "unavailable",
        "sandbox_enforcement": "unavailable",
        "command_scope_digest": command_scope_digest(
            &input.argv,
            input.effective_cwd(),
            input.effective_timeout_seconds(),
            &input.sandbox_mode(),
            &input.network_access(),
        ),
        "command_provenance": "agent_requested",
    });
    output
}
