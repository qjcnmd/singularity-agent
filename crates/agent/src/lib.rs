#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelMessage, ModelPreferences,
    ModelPurpose, ModelRole, ModelToolCall, ModelToolParseStatus, ModelToolSchema,
    ModelTurnRequest, ModelTurnStatus, Provider, provider_error_response,
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
const NATIVE_AGENT_LOOP_READY_REASON: &str = "AgentLoop uses automatic Windows elevated sandbox setup with restricted-token fallback only for network-enabled profiles";
#[cfg(not(windows))]
const NATIVE_AGENT_LOOP_UNSUPPORTED_PLATFORM_REASON: &str =
    "AgentLoop requires the Windows restricted-token command sandbox";
const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 4;
const AGENT_DEVELOPER_INSTRUCTIONS: &str = "You are a coding agent working in the current workspace. Use the available tools to inspect real files before making claims. Make requested changes through tools, keep all writes inside the workspace, and run a relevant verification command after the last workspace mutation. Report only work and verification that actually completed. Read-only questions may be answered without changing files or running verification.";
const APPROXIMATE_ASCII_CHARS_PER_TOKEN: usize = 4;
const USER_MESSAGE_ROLE: &str = "user";
const ASSISTANT_MESSAGE_ROLE: &str = "assistant";
const MODEL_MESSAGE_FRAMING_TOKENS: u32 = 4;
const MODEL_REQUEST_FIXED_OVERHEAD_TOKENS: u32 = 256;
const EMPTY_FINAL_ANSWER_ERROR: &str = "empty final answer";
const CURRENT_TURN_CONTEXT_OVERFLOW_ERROR: &str = "current turn exceeds the model context budget";
const MODEL_REQUEST_CONTEXT_OVERFLOW_ERROR: &str = "model request exceeds the model context budget";
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentRunStatus {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub audit_events: Vec<Value>,
    pub trace_path: Option<String>,
    pub verification: AgentVerification,
    pub error: Option<String>,
}

impl AgentRunStatus {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentStatus::Failed,
            completed: false,
            final_answer: None,
            run_id: None,
            session_id: None,
            task_id: None,
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            audit_events: Vec::new(),
            trace_path: None,
            verification: AgentVerification::default(),
            error: Some(message.into()),
        }
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
                reason: NATIVE_AGENT_LOOP_READY_REASON.to_string(),
                blockers: Vec::new(),
            }
        }
        #[cfg(not(windows))]
        {
            Self {
                available: false,
                status: AgentStatus::Blocked,
                reason: NATIVE_AGENT_LOOP_UNSUPPORTED_PLATFORM_REASON.to_string(),
                blockers: vec![STRICT_COMMAND_SANDBOX_UNSUPPORTED_PLATFORM.to_string()],
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentLoopStep {
    LoadTurn,
    AssembleContext,
    CallModel,
    AdmitToolCalls,
    ExecuteApprovedTools,
    AppendToolResults,
    RepairOnFailure,
    FinalizeReport,
    PersistItemsTraceArtifacts,
    HandleInterrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopPlan {
    pub steps: Vec<AgentLoopStep>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopInput {
    pub thread_id: String,
    pub turn_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
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
            run_id: turn_id.clone(),
            session_id: turn_id.clone(),
            task_id: turn_id.clone(),
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
    pub pending_tool_calls: Vec<PendingToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub tool_repairs: Vec<ToolRepair>,
    pub verification: AgentVerification,
    pub error: Option<String>,
}

impl AgentLoopResult {
    pub fn to_run_status(&self, input: &AgentLoopInput) -> AgentRunStatus {
        AgentRunStatus {
            status: self.status.clone(),
            completed: self.completed,
            final_answer: self.final_answer.clone(),
            run_id: Some(input.run_id.clone()),
            session_id: Some(input.session_id.clone()),
            task_id: Some(input.task_id.clone()),
            model_turns: self.model_turns,
            tool_calls: self.tool_calls,
            approval_count: self.approval_count,
            audit_events: audit_events_from_tool_results(&self.tool_results),
            trace_path: None,
            verification: self.verification.clone(),
            error: self.error.clone(),
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

#[derive(Default)]
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

struct AgentLoopState {
    messages: Vec<ModelMessage>,
    tool_results: Vec<ToolResult>,
    tool_repairs: Vec<ToolRepair>,
    approval_requests: Vec<ApprovalRequest>,
    pending_tool_calls: Vec<PendingToolCall>,
    used_approval_grants: HashSet<String>,
    prior_approval_count: u32,
    completion: CompletionTracker,
    last_completion_error: Option<String>,
}

impl AgentLoopState {
    fn new(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            tool_results: Vec::new(),
            tool_repairs: Vec::new(),
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            used_approval_grants: HashSet::new(),
            prior_approval_count: 0,
            completion: CompletionTracker::default(),
            last_completion_error: None,
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
            tool_results: self.tool_results,
            tool_repairs: self.tool_repairs,
            verification: self.completion.summary(),
            error,
        }
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

    pub fn integration_plan() -> AgentLoopPlan {
        AgentLoopPlan {
            steps: vec![
                AgentLoopStep::LoadTurn,
                AgentLoopStep::AssembleContext,
                AgentLoopStep::CallModel,
                AgentLoopStep::AdmitToolCalls,
                AgentLoopStep::ExecuteApprovedTools,
                AgentLoopStep::AppendToolResults,
                AgentLoopStep::RepairOnFailure,
                AgentLoopStep::FinalizeReport,
                AgentLoopStep::PersistItemsTraceArtifacts,
                AgentLoopStep::HandleInterrupt,
            ],
            blockers: Vec::new(),
        }
    }

    pub fn run(&self, input: &AgentLoopInput) -> AgentLoopResult {
        let context = assemble_context_items(
            &input.input,
            context_input_token_budget(input, &self.tool_broker),
        );
        if current_turn_excluded(input, &context) {
            return context_overflow_result();
        }
        let state = AgentLoopState::new(model_messages_from_input(input, &context));
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, 0, None);
        }
        self.continue_run(input, &context, state)
    }

    fn continue_run(
        &self,
        input: &AgentLoopInput,
        context: &ContextBundle,
        mut state: AgentLoopState,
    ) -> AgentLoopResult {
        let max_turns = input.max_turns.max(1);
        for turn_index in 0..max_turns {
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index, None);
            }
            if !model_request_fits_context(input, &self.tool_broker, &state.messages) {
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
                context,
                turn_index,
                state.messages.clone(),
            );
            let response = match self.provider.complete(&request, &self.cancellation) {
                Ok(response) => response,
                Err(error) => provider_error_response(&request, error),
            };
            if self.is_cancelled(input) {
                return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
            }
            if response.status != ModelTurnStatus::Success {
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    turn_index + 1,
                    response.error.map(|error| error.message),
                );
            }
            if response.tool_calls.is_empty() {
                let final_answer = assistant_message_text(response.assistant_message.as_ref());
                if final_answer.trim().is_empty() {
                    return state.finish(
                        AgentStatus::Failed,
                        false,
                        None,
                        turn_index + 1,
                        Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    );
                }
                if state.completion.allows_final() {
                    return state.finish(
                        AgentStatus::Completed,
                        true,
                        Some(final_answer),
                        turn_index + 1,
                        None,
                    );
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
            let assistant_tool_message = response
                .assistant_message
                .as_ref()
                .filter(|message| !message.tool_calls.is_empty())
                .cloned()
                .unwrap_or_else(|| ModelMessage::assistant_tool_calls(response.tool_calls.clone()));
            state.messages.push(assistant_tool_message);
            for provider_call in &response.tool_calls {
                let (bound_call, forced_decision) =
                    match bind_tool_call_to_profile(provider_call, &self.policy.profile) {
                        Ok(call) => (call, None),
                        Err(reason) => (
                            provider_call.clone(),
                            Some(ToolBrokerDecision::deny(reason)),
                        ),
                    };
                let call = &bound_call;
                if self.is_cancelled(input) {
                    return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
                }
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
                    let tool_result = self.execute_tool(input, call, decision);
                    state.tool_results.push(tool_result);
                    return state.finish(AgentStatus::Blocked, false, None, turn_index + 1, None);
                }
                let tool_result = self.execute_tool(input, call, decision);
                let failed_tool_result = !tool_result.ok;
                state.completion.observe(&tool_result);
                state.tool_results.push(tool_result.clone());
                state.messages.push(tool_result_message(&tool_result));
                if self.is_cancelled(input) {
                    return state.finish(AgentStatus::Cancelled, false, None, turn_index + 1, None);
                }
                if failed_tool_result {
                    if is_repairable_tool_result(&tool_result) {
                        state
                            .tool_repairs
                            .push(tool_repair(input, turn_index, &tool_result));
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
    ) -> AgentLoopResult {
        if self.is_cancelled(input) {
            return AgentLoopState::new(Vec::new()).finish(
                AgentStatus::Cancelled,
                false,
                None,
                0,
                None,
            );
        }
        let call = match pending
            .to_model_tool_call()
            .map_err(|error| format!("invalid pending tool call arguments: {error}"))
            .and_then(|call| bind_tool_call_to_profile(&call, &self.policy.profile))
        {
            Ok(call) => call,
            Err(error) => {
                return AgentLoopState::new(Vec::new()).finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    0,
                    Some(error),
                );
            }
        };
        let context = assemble_context_items(
            &input.input,
            context_input_token_budget(input, &self.tool_broker),
        );
        if current_turn_excluded(input, &context) {
            return context_overflow_result();
        }
        let mut state = AgentLoopState::new(model_messages_from_input(input, &context));
        state
            .messages
            .push(ModelMessage::assistant_tool_calls(vec![call.clone()]));
        state.prior_approval_count = 1;
        let mut used_approval_grants = HashSet::new();
        let decision = self.tool_decision(input, &call, &mut used_approval_grants);
        if !matches!(decision, ToolBrokerDecision::Approved { .. }) {
            return state.finish(
                AgentStatus::Failed,
                false,
                None,
                0,
                Some("pending tool call approval did not match".to_string()),
            );
        }
        state.used_approval_grants = used_approval_grants;
        let tool_result = self.execute_tool(input, &call, decision);
        let failed_tool_result = !tool_result.ok;
        state.completion.observe(&tool_result);
        state.messages.push(tool_result_message(&tool_result));
        state.tool_results.push(tool_result.clone());
        if self.is_cancelled(input) {
            return state.finish(AgentStatus::Cancelled, false, None, 0, None);
        }
        if failed_tool_result {
            if is_repairable_tool_result(&tool_result) {
                state.tool_repairs.push(tool_repair(input, 0, &tool_result));
            } else {
                let error_code = tool_result
                    .error_code
                    .as_deref()
                    .unwrap_or("tool_execution_failed");
                return state.finish(
                    AgentStatus::Failed,
                    false,
                    None,
                    0,
                    Some(format!("tool execution failed: {error_code}")),
                );
            }
        }
        self.continue_run(input, &context, state)
    }

    fn is_cancelled(&self, input: &AgentLoopInput) -> bool {
        input.interrupted || self.cancellation.is_cancelled()
    }

    fn tool_decision(
        &self,
        input: &AgentLoopInput,
        call: &ModelToolCall,
        used_approval_grants: &mut HashSet<String>,
    ) -> ToolBrokerDecision {
        if call.parse_status != ModelToolParseStatus::Valid || !call.arguments.is_object() {
            return ToolBrokerDecision::deny("invalid tool call arguments");
        }
        let request_id = approval_request_id(input, call);
        let resources = permission_resources_for_tool(call);
        let permission = self.tool_permission_decision(call);
        if !matches!(permission.outcome, PermissionDecisionOutcome::Deny)
            && let Some(grant) = input.approval_grants.iter().find(|grant| {
                grant.request_id == request_id
                    && grant.tool_name == call.tool_name
                    && grant.resources == resources
                    && matches!(grant.outcome, ApprovalOutcome::Allow)
                    && !used_approval_grants.contains(&grant.request_id)
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
        input: &AgentLoopInput,
        call: &ModelToolCall,
        decision: ToolBrokerDecision,
    ) -> ToolResult {
        let envelope = ToolCallRequest::new(
            input.run_id.clone(),
            input.session_id.clone(),
            input.task_id.clone(),
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            call.raw_arguments.clone(),
        );
        let executor_decision = decision.clone();
        let mut result = self
            .tool_broker
            .execute(&envelope, decision.clone(), |_envelope| {
                self.execute_workspace_tool(call, &executor_decision)
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
    pub digest: String,
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
            digest: "user_input".to_string(),
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
        let digest = format!("history_{role}:{item_id}");
        Self {
            item_id,
            role: role.to_string(),
            token_count: approximate_token_count(&content),
            content,
            priority: AgentContextItemPriority::History,
            public: true,
            evaluator_only: false,
            digest,
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

fn context_input_token_budget(input: &AgentLoopInput, loop_tools: &ToolBroker) -> u32 {
    let developer_instruction_tokens = approximate_token_count(&developer_instructions(input));
    let tool_tokens = serde_json::to_string(&model_tool_schemas(loop_tools))
        .map_or(u32::MAX, |tools| approximate_token_count(&tools));
    let message_count = u32::try_from(input.input.len().saturating_add(1)).unwrap_or(u32::MAX);
    let framing_tokens = message_count.saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    let output_tokens = input
        .model_preferences
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let reserved_tokens = output_tokens
        .saturating_add(MODEL_REQUEST_FIXED_OVERHEAD_TOKENS)
        .saturating_add(developer_instruction_tokens)
        .saturating_add(tool_tokens)
        .saturating_add(framing_tokens);
    DEFAULT_MAX_CONTEXT_TOKENS.saturating_sub(reserved_tokens)
}

fn model_request_fits_context(
    input: &AgentLoopInput,
    loop_tools: &ToolBroker,
    messages: &[ModelMessage],
) -> bool {
    let projected_messages = messages
        .iter()
        .map(|message| {
            let content = message
                .content
                .iter()
                .filter_map(|block| block.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
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
    let output_tokens = input
        .model_preferences
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let message_framing = u32::try_from(messages.len())
        .unwrap_or(u32::MAX)
        .saturating_mul(MODEL_MESSAGE_FRAMING_TOKENS);
    payload_tokens
        .saturating_add(output_tokens)
        .saturating_add(message_framing)
        .saturating_add(MODEL_REQUEST_FIXED_OVERHEAD_TOKENS)
        <= DEFAULT_MAX_CONTEXT_TOKENS
}
pub fn assemble_context_items(items: &[AgentContextItem], max_tokens: u32) -> ContextBundle {
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
    let mut digest_parts = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !included_indices.contains(&index) {
            excluded_item_ids.push(item.item_id.clone());
            continue;
        }
        included_item_ids.push(item.item_id.clone());
        digest_parts.push(item.digest.clone());
        messages.push(json!({
            "role": item.role,
            "content": item.content,
        }));
    }

    ContextBundle {
        bundle_id: "rust_context_bundle".to_string(),
        run_id: String::new(),
        task_id: String::new(),
        phase_id: "context".to_string(),
        model: String::new(),
        provider: String::new(),
        messages,
        included_item_ids,
        excluded_item_ids,
        budget: json!({
            "model_context_window": DEFAULT_MAX_CONTEXT_TOKENS,
            "input_token_budget": max_tokens,
            "message_tokens": used_tokens,
        }),
        compression_snapshot_id: None,
        retrieval_query: None,
        render_policy: json!({
            "include_raw_tool_outputs": false,
            "redact_sensitive": true,
        }),
        created_at: String::new(),
        bundle_digest: digest_parts.join(":"),
        metadata: json!({}),
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

fn context_overflow_result() -> AgentLoopResult {
    AgentLoopResult {
        status: AgentStatus::Failed,
        completed: false,
        final_answer: None,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        approval_requests: Vec::new(),
        pending_tool_calls: Vec::new(),
        tool_results: Vec::new(),
        tool_repairs: Vec::new(),
        verification: AgentVerification::default(),
        error: Some(CURRENT_TURN_CONTEXT_OVERFLOW_ERROR.to_string()),
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundle {
    pub bundle_id: String,
    pub run_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub model: String,
    pub provider: String,
    pub messages: Vec<Value>,
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
    pub compression_snapshot_id: Option<String>,
    pub retrieval_query: Option<String>,
    pub render_policy: Value,
    pub created_at: String,
    pub bundle_digest: String,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolRepair {
    pub repair_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub failed_tool_call_id: String,
    pub failure_kind: String,
    pub next_action: String,
    pub failed_result: Value,
    pub recovery_report: Value,
    pub repair_contract: Value,
    pub created_at: String,
    pub metadata: Value,
}

fn model_turn_request(
    loop_tools: &ToolBroker,
    input: &AgentLoopInput,
    context: &ContextBundle,
    turn_index: u32,
    messages: Vec<ModelMessage>,
) -> ModelTurnRequest {
    ModelTurnRequest {
        purpose: ModelPurpose::PlanNextAction,
        request_id: format!("model_request_{}_{}", input.turn_id, turn_index),
        run_id: input.run_id.clone(),
        session_id: input.session_id.clone(),
        task_id: input.task_id.clone(),
        phase_id: "model".to_string(),
        action_id: format!("model_action_{}_{}", input.turn_id, turn_index),
        messages,
        tools: model_tool_schemas(loop_tools),
        tool_choice: Default::default(),
        model_preferences: input.model_preferences.clone(),
        context_metadata: context_metadata(context),
        policy_metadata: json!({
            "approval_grants": input
                .approval_grants
                .iter()
                .map(|grant| json!({
                    "request_id": &grant.request_id,
                    "tool_name": &grant.tool_name,
                    "resources": &grant.resources,
                    "outcome": grant.outcome,
                }))
                .collect::<Vec<_>>(),
        }),
        trace_metadata: json!({
            "turn_id": &input.turn_id,
            "run_id": &input.run_id,
            "session_id": &input.session_id,
            "task_id": &input.task_id,
        }),
    }
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
                capability_tags: Vec::new(),
                risk_tags: Vec::new(),
                metadata: json!({}),
            })
        })
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

fn context_metadata(context: &ContextBundle) -> Value {
    json!({
        "bundle_id": &context.bundle_id,
        "included_item_ids": &context.included_item_ids,
        "excluded_item_ids": &context.excluded_item_ids,
        "budget": &context.budget,
        "render_policy": &context.render_policy,
        "bundle_digest": &context.bundle_digest,
    })
}

fn assistant_message_text(message: Option<&ModelMessage>) -> String {
    message
        .into_iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| block.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
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
    }
    audit["approval_policy"] = serde_json::to_value(approval_policy).unwrap_or(json!("unknown"));
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

fn tool_repair(input: &AgentLoopInput, turn_index: u32, tool_result: &ToolResult) -> ToolRepair {
    let failure_kind = tool_result
        .error_code
        .clone()
        .unwrap_or_else(|| "tool_result_failed".to_string());
    ToolRepair {
        repair_id: format!(
            "repair_{}_{}_{}",
            input.turn_id, tool_result.tool_call_id, turn_index
        ),
        run_id: input.run_id.clone(),
        session_id: input.session_id.clone(),
        task_id: input.task_id.clone(),
        phase_id: "tool_repair".to_string(),
        failed_tool_call_id: tool_result.tool_call_id.clone(),
        failure_kind,
        next_action: "request_model".to_string(),
        failed_result: tool_result.to_message_payload(),
        recovery_report: json!({
            "status": "queued",
            "reason": "repairable_tool_result",
        }),
        repair_contract: json!({
            "max_turns": input.max_turns,
            "retry_after_turn": turn_index + 1,
        }),
        created_at: String::new(),
        metadata: json!({
            "tool_name": &tool_result.tool_name,
        }),
    }
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
    format!("approval_{}_{}", input.turn_id, call.tool_call_id)
}

fn bind_tool_call_to_profile(
    call: &ModelToolCall,
    profile: &PermissionProfile,
) -> Result<ModelToolCall, String> {
    if call.tool_name != TOOL_COMMAND {
        return Ok(call.clone());
    }
    let mut input = command_tool_input(&call.arguments).map_err(|error| error.to_string())?;
    let cwd = input.effective_cwd().to_string();
    let timeout_seconds = input.effective_timeout_seconds();
    let (sandbox_mode, network_access) = effective_command_policy(profile, &input)?;
    input.cwd = Some(cwd);
    input.timeout_seconds = Some(timeout_seconds);
    input.sandbox_mode = Some(sandbox_mode);
    input.network_access = Some(network_access);
    let arguments = serde_json::to_value(input).map_err(|error| error.to_string())?;
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
    ToolOutput::failure(error_code, json!({"summary": error.to_string()}))
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
