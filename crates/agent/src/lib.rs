#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_model::{
    ModelMessage, ModelPreferences, ModelPurpose, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelToolSchema, ModelTurnRequest, ModelTurnStatus, Provider, provider_error_response,
};
use singularity_policy::{
    ApprovalRequest, PermissionDecisionOutcome, PermissionOperation, PermissionRequest,
    PolicyEngine,
};
use singularity_tools::{
    EditToolInput, GrepToolInput, ListToolInput, ReadToolInput, ToolBroker, ToolBrokerDecision,
    ToolCallEnvelope, ToolObservation, ToolResult, WorkspacePatch, WorkspaceToolError,
    WorkspaceTools,
};
use thiserror::Error;

const SIDECAR_METHOD_RUN: &str = "agent/run";
const SIDECAR_METHOD_RESUME: &str = "agent/resume";
const SIDECAR_METHOD_CANCEL: &str = "agent/cancel";
const SIDECAR_METHOD_STATUS: &str = "agent/status";
const SIDECAR_METHOD_HEALTH: &str = "agent/health";
const SIDECAR_COMPONENT: &str = "python_sidecar";
const DEFAULT_PYTHON_BIN: &str = "python";
const DEFAULT_SIDECAR_MODULE: &str = "singularity.agent_host.sidecar";
const DEFAULT_SIDECAR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const NATIVE_AGENT_LOOP_UNAVAILABLE_REASON: &str =
    "native Rust AgentLoop is partially migrated; command/sandbox/eval remain blocked";
const NATIVE_AGENT_LOOP_MISSING_BOUNDARIES: [&str; 6] = [
    "planner_step",
    "compaction_executor",
    "tool_repair_runtime",
    "completion_gate",
    "strict_command_sandbox",
    "rust_evaluation_runner",
];
const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 4;
const COMPLETION_GATE_NOT_MIGRATED: &str = "completion_gate_not_migrated";
const TOOL_READ: &str = "builtin.read";
const TOOL_LIST: &str = "builtin.list";
const TOOL_GREP: &str = "builtin.grep";
const TOOL_EDIT: &str = "builtin.edit";
const TOOL_PATCH: &str = "builtin.patch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentHostStatus {
    NotMigrated,
    Running,
    CancelRequested,
    Completed,
    Blocked,
    Cancelled,
    Failed,
}

impl AgentHostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotMigrated => "not_migrated",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl From<&str> for AgentHostStatus {
    fn from(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "cancel_requested" => Self::CancelRequested,
            "completed" => Self::Completed,
            "blocked" => Self::Blocked,
            "cancelled" | "canceled" => Self::Cancelled,
            "failed" | "max_turns_exceeded" => Self::Failed,
            "not_migrated" => Self::NotMigrated,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopStatusBridge {
    pub status: AgentHostStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub events: Vec<SidecarRunEvent>,
    pub trace_path: Option<String>,
    pub error: Option<String>,
}

impl AgentLoopStatusBridge {
    pub fn not_migrated() -> Self {
        Self {
            status: AgentHostStatus::NotMigrated,
            completed: false,
            final_answer: None,
            run_id: None,
            session_id: None,
            task_id: None,
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            events: Vec::new(),
            trace_path: None,
            error: None,
        }
    }

    pub fn from_sidecar(result: PythonSidecarRunResult) -> Self {
        let status = AgentHostStatus::from(result.status.as_str());
        Self {
            completed: status == AgentHostStatus::Completed,
            final_answer: result.final_answer,
            run_id: Some(result.run_id),
            session_id: Some(result.session_id),
            task_id: Some(result.task_id),
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            events: result.events,
            trace_path: result.trace_path,
            error: None,
            status,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentHostStatus::Failed,
            completed: false,
            final_answer: None,
            run_id: None,
            session_id: None,
            task_id: None,
            model_turns: 0,
            tool_calls: 0,
            approval_count: 0,
            events: Vec::new(),
            trace_path: None,
            error: Some(message.into()),
        }
    }

    pub fn with_status(mut self, status: AgentHostStatus) -> Self {
        self.completed = status == AgentHostStatus::Completed;
        self.status = status;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopCapability {
    pub available: bool,
    pub status: AgentHostStatus,
    pub reason: String,
    pub missing_boundaries: Vec<String>,
}

impl AgentLoopCapability {
    pub fn current() -> Self {
        Self {
            available: false,
            status: AgentHostStatus::NotMigrated,
            reason: NATIVE_AGENT_LOOP_UNAVAILABLE_REASON.to_string(),
            missing_boundaries: NATIVE_AGENT_LOOP_MISSING_BOUNDARIES
                .iter()
                .map(|boundary| (*boundary).to_string())
                .collect(),
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
    AppendObservations,
    RepairOnFailure,
    FinalizeReport,
    PersistItemsTraceArtifacts,
    HandleInterrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopPlan {
    pub steps: Vec<AgentLoopStep>,
    pub merge_requirements: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopInput {
    pub thread_id: String,
    pub turn_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub model_preferences: ModelPreferences,
    pub input: Vec<AgentContextItem>,
    pub interrupted: bool,
    pub max_turns: u32,
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
            input: vec![AgentContextItem::user("input_1", goal.into())],
            interrupted: false,
            max_turns: DEFAULT_MAX_AGENT_LOOP_TURNS,
        }
    }

    pub fn with_model_name(mut self, model_name: Option<String>) -> Self {
        self.model_preferences.model_name = model_name;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopResult {
    pub status: AgentHostStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub approval_requests: Vec<ApprovalRequest>,
    pub observations: Vec<ToolObservation>,
    pub error: Option<String>,
}

impl AgentLoopResult {
    pub fn bridge(&self, input: &AgentLoopInput) -> AgentLoopStatusBridge {
        AgentLoopStatusBridge {
            status: self.status.clone(),
            completed: self.completed,
            final_answer: self.final_answer.clone(),
            run_id: Some(input.run_id.clone()),
            session_id: Some(input.session_id.clone()),
            task_id: Some(input.task_id.clone()),
            model_turns: self.model_turns,
            tool_calls: self.tool_calls,
            approval_count: self.approval_count,
            events: Vec::new(),
            trace_path: None,
            error: self.error.clone(),
        }
    }
}

pub struct AgentLoop<P> {
    provider: P,
    tool_broker: ToolBroker,
    policy: PolicyEngine,
    workspace_tools: Option<WorkspaceTools>,
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
        }
    }

    pub fn with_workspace_tools(mut self, workspace_tools: WorkspaceTools) -> Self {
        self.workspace_tools = Some(workspace_tools);
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
                AgentLoopStep::AppendObservations,
                AgentLoopStep::RepairOnFailure,
                AgentLoopStep::FinalizeReport,
                AgentLoopStep::PersistItemsTraceArtifacts,
                AgentLoopStep::HandleInterrupt,
            ],
            merge_requirements: NATIVE_AGENT_LOOP_MISSING_BOUNDARIES
                .iter()
                .map(|boundary| (*boundary).to_string())
                .collect(),
        }
    }

    pub fn run(&self, input: &AgentLoopInput) -> AgentLoopResult {
        if input.interrupted {
            return AgentLoopResult {
                status: AgentHostStatus::Cancelled,
                completed: false,
                final_answer: None,
                model_turns: 0,
                tool_calls: 0,
                approval_count: 0,
                approval_requests: Vec::new(),
                observations: Vec::new(),
                error: None,
            };
        }
        let mut observations = Vec::new();
        let mut approval_requests = Vec::new();
        let mut messages = model_messages_from_context(&input.input);
        let max_turns = input.max_turns.max(1);
        for turn_index in 0..max_turns {
            let request =
                model_turn_request(&self.tool_broker, input, turn_index, messages.clone());
            let response = match self.provider.complete(&request) {
                Ok(response) => response,
                Err(error) => provider_error_response(&request, error),
            };
            if response.status != ModelTurnStatus::Success {
                return AgentLoopResult {
                    status: AgentHostStatus::Failed,
                    completed: false,
                    final_answer: None,
                    model_turns: turn_index + 1,
                    tool_calls: observations.len() as u32,
                    approval_count: 0,
                    approval_requests,
                    observations,
                    error: response.error.map(|error| error.message),
                };
            }
            if response.tool_calls.is_empty() {
                return AgentLoopResult {
                    status: AgentHostStatus::Blocked,
                    completed: false,
                    final_answer: None,
                    model_turns: turn_index + 1,
                    tool_calls: observations.len() as u32,
                    approval_count: 0,
                    approval_requests,
                    observations,
                    error: Some(COMPLETION_GATE_NOT_MIGRATED.to_string()),
                };
            }
            for call in &response.tool_calls {
                let decision = self.tool_decision(input, call);
                if let ToolBrokerDecision::Ask {
                    approval_request_id,
                    reason,
                } = &decision
                {
                    approval_requests.push(approval_request(
                        input,
                        approval_request_id,
                        &call.tool_name,
                        reason,
                    ));
                    let observation = self.execute_tool(input, call, decision);
                    observations.push(observation);
                    return AgentLoopResult {
                        status: AgentHostStatus::Blocked,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: observations.len() as u32,
                        approval_count: approval_requests.len() as u32,
                        approval_requests,
                        observations,
                        error: None,
                    };
                }
                let observation = self.execute_tool(input, call, decision);
                let failed_observation = !observation.ok;
                observations.push(observation.clone());
                messages.push(observation_message(&observation));
                if failed_observation {
                    let error_code = observation
                        .error_code
                        .as_deref()
                        .unwrap_or("tool_execution_failed");
                    return AgentLoopResult {
                        status: AgentHostStatus::Failed,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: observations.len() as u32,
                        approval_count: 0,
                        approval_requests,
                        observations,
                        error: Some(format!("tool execution failed: {error_code}")),
                    };
                }
            }
        }
        AgentLoopResult {
            status: AgentHostStatus::Failed,
            completed: false,
            final_answer: None,
            model_turns: max_turns,
            tool_calls: observations.len() as u32,
            approval_count: 0,
            approval_requests,
            observations,
            error: Some("max turns exceeded".to_string()),
        }
    }

    fn tool_decision(&self, input: &AgentLoopInput, call: &ModelToolCall) -> ToolBrokerDecision {
        if call.parse_status != ModelToolParseStatus::Valid || !call.arguments.is_object() {
            return ToolBrokerDecision::deny("invalid tool call arguments");
        }
        let operation = permission_operation_for_tool(&call.tool_name);
        let resource = permission_resource_for_tool(call);
        let mut request = PermissionRequest::new(call.tool_name.clone(), operation, resource);
        if tool_call_targets_sensitive_resource(call) {
            request = request.with_sensitive_resource();
        }
        let permission = self.policy.evaluate(&request);
        match permission.outcome {
            PermissionDecisionOutcome::Allow => ToolBrokerDecision::Allow,
            PermissionDecisionOutcome::Deny => ToolBrokerDecision::deny(permission.reason),
            PermissionDecisionOutcome::Ask => {
                ToolBrokerDecision::ask(format!("approval_{}", input.turn_id), permission.reason)
            }
        }
    }

    fn execute_tool(
        &self,
        input: &AgentLoopInput,
        call: &ModelToolCall,
        decision: ToolBrokerDecision,
    ) -> ToolObservation {
        let envelope = ToolCallEnvelope::new(
            input.run_id.clone(),
            input.session_id.clone(),
            input.task_id.clone(),
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            call.raw_arguments.clone(),
        );
        let executor_decision = decision.clone();
        self.tool_broker.execute(&envelope, decision, |_envelope| {
            self.execute_workspace_tool(call, &executor_decision)
        })
    }

    fn execute_workspace_tool(
        &self,
        call: &ModelToolCall,
        decision: &ToolBrokerDecision,
    ) -> ToolResult {
        let Some(workspace_tools) = &self.workspace_tools else {
            return ToolResult::failure(
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
            _ => Ok(ToolResult::failure(
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
pub struct EvaluationDiagnostics {
    pub base_verification_passed: Option<bool>,
    pub sandbox_required: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationRunReport {
    pub evaluation_passed: bool,
    pub agent_completed: bool,
    pub tests_passed: bool,
    pub public_verification_passed: bool,
    pub hidden_verification_passed: bool,
    pub local_process_fallback_count: u32,
    pub diagnostics: EvaluationDiagnostics,
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
    pub safe_for_model: bool,
    pub evaluator_only: bool,
    pub digest: String,
}

impl AgentContextItem {
    pub fn user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            item_id: item_id.into(),
            role: "user".to_string(),
            content,
            priority: AgentContextItemPriority::CurrentTurn,
            token_count: 1,
            safe_for_model: true,
            evaluator_only: false,
            digest: "user_input".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlannerNextAction {
    ResumePendingApproval,
    ExecutePendingTool,
    Final,
    AskUser,
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepairNextAction {
    RepairThenVerify,
    RequestModel,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompletionGateInput {
    pub verification_passed: bool,
    pub unresolved_failures: Vec<String>,
    pub interrupted: bool,
}

pub fn assemble_context_items(
    items: &[AgentContextItem],
    max_tokens: u32,
) -> ContextAssemblyBoundary {
    let mut candidates: Vec<(usize, &AgentContextItem)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.safe_for_model && !item.evaluator_only)
        .collect();
    candidates.sort_by_key(|(index, item)| (item.priority.rank(), *index));

    let mut used_tokens = 0;
    let mut included_item_ids = Vec::new();
    let mut excluded_item_ids: Vec<String> = items
        .iter()
        .filter(|item| !item.safe_for_model || item.evaluator_only)
        .map(|item| item.item_id.clone())
        .collect();
    let mut messages = Vec::new();
    let mut digest_parts = Vec::new();

    for (_, item) in candidates {
        if used_tokens + item.token_count > max_tokens {
            excluded_item_ids.push(item.item_id.clone());
            continue;
        }
        used_tokens += item.token_count;
        included_item_ids.push(item.item_id.clone());
        digest_parts.push(item.digest.clone());
        messages.push(json!({
            "role": item.role,
            "content": item.content,
        }));
    }

    ContextAssemblyBoundary {
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
            "model_context_window": max_tokens,
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

pub fn planner_next_action(state: &PlannerStateBoundary) -> PlannerNextAction {
    if state
        .open_actions
        .iter()
        .any(|action| action_matches(action, "approval", "pending"))
    {
        return PlannerNextAction::ResumePendingApproval;
    }
    if state
        .open_actions
        .iter()
        .any(|action| action_matches(action, "tool", "pending"))
    {
        return PlannerNextAction::ExecutePendingTool;
    }
    if state.current_phase == "finalizing" {
        return PlannerNextAction::Final;
    }
    if !state.blocked_actions.is_empty() {
        return PlannerNextAction::AskUser;
    }
    PlannerNextAction::Continue
}

pub fn repair_next_action(repair: &ToolCallRepairBoundary) -> RepairNextAction {
    match repair.next_action.as_str() {
        "repair_then_verify" => RepairNextAction::RepairThenVerify,
        "request_model" => RepairNextAction::RequestModel,
        _ => RepairNextAction::Blocked,
    }
}

pub fn completion_gate_allows_final(input: &CompletionGateInput) -> bool {
    input.verification_passed && input.unresolved_failures.is_empty() && !input.interrupted
}

pub fn final_mapping_from_status(
    mapping_id: &str,
    run_id: &str,
    session_id: &str,
    task_id: &str,
    status: AgentHostStatus,
    final_answer: &str,
) -> FinalizationMappingBoundary {
    let run_status = match status {
        AgentHostStatus::Completed => "completed",
        AgentHostStatus::Blocked => "blocked",
        AgentHostStatus::Cancelled | AgentHostStatus::CancelRequested => "interrupted",
        AgentHostStatus::Failed => "failed",
        AgentHostStatus::Running | AgentHostStatus::NotMigrated => "running",
    };
    FinalizationMappingBoundary {
        mapping_id: mapping_id.to_string(),
        run_id: run_id.to_string(),
        session_id: session_id.to_string(),
        task_id: task_id.to_string(),
        phase_id: "finalizing".to_string(),
        agent_loop_status: status.as_str().to_string(),
        run_status: run_status.to_string(),
        final_report_status: run_status.to_string(),
        completion_status: run_status.to_string(),
        final_answer: final_answer.to_string(),
        final_report: json!({"status": run_status}),
        completion_assessment: json!({"status": run_status}),
        contract_satisfaction: json!({"satisfied": status == AgentHostStatus::Completed}),
        created_at: String::new(),
        metadata: json!({}),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSidecarConfig {
    pub python_bin: String,
    pub module: String,
    pub project_root: PathBuf,
    pub python_path: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl PythonSidecarConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            python_bin: DEFAULT_PYTHON_BIN.to_string(),
            module: DEFAULT_SIDECAR_MODULE.to_string(),
            project_root: project_root.into(),
            python_path: None,
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PythonSidecarRunResult {
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer: Option<String>,
    #[serde(default)]
    pub events: Vec<SidecarRunEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PythonSidecarStatus {
    pub run_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_path: Option<String>,
    #[serde(default)]
    pub events: Vec<SidecarRunEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SidecarRunEvent {
    pub event_id: String,
    pub event_type: String,
    pub summary: String,
    pub component: String,
    pub severity: String,
    pub sequence: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlannerStateBoundary {
    pub task_id: String,
    pub current_phase: String,
    pub status: String,
    pub current_plan: Vec<Value>,
    pub completion_criteria: Value,
    pub open_actions: Vec<Value>,
    pub blocked_actions: Vec<Value>,
    pub risk_escalations: Vec<Value>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextAssemblyBoundary {
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
pub struct ContextSummaryEnvelopeBoundary {
    pub version: u32,
    pub summary_id: String,
    pub summary_payload: Value,
    pub source_item_ids: Vec<String>,
    pub cache_attribution: Value,
    pub previous_summary_digest: Option<String>,
    pub summary_digest: String,
    pub rendered_summary: String,
    pub created_at: String,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallRepairBoundary {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FinalizationMappingBoundary {
    pub mapping_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub agent_loop_status: String,
    pub run_status: String,
    pub final_report_status: String,
    pub completion_status: String,
    pub final_answer: String,
    pub final_report: Value,
    pub completion_assessment: Value,
    pub contract_satisfaction: Value,
    pub created_at: String,
    pub metadata: Value,
}

#[derive(Debug)]
pub struct PythonSidecarClient {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<Result<String, String>>,
    stdout_reader: Option<JoinHandle<()>>,
    next_id: i64,
    response_timeout: Duration,
}

impl PythonSidecarClient {
    pub fn spawn(config: &PythonSidecarConfig) -> Result<Self, String> {
        Self::spawn_with_response_timeout(config, DEFAULT_SIDECAR_RESPONSE_TIMEOUT)
    }

    pub fn spawn_with_response_timeout(
        config: &PythonSidecarConfig,
        response_timeout: Duration,
    ) -> Result<Self, String> {
        let mut command = Command::new(&config.python_bin);
        command
            .args(["-m", &config.module])
            .env("SINGULARITY_SIDECAR_PROJECT_ROOT", &config.project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(python_path) = &config.python_path {
            command.env("PYTHONPATH", python_path);
        }
        for (name, value) in &config.env {
            command.env(name, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start Python sidecar: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Python sidecar stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Python sidecar stdout unavailable".to_string())?;
        let (stdout, stdout_reader) = spawn_stdout_reader(stdout);
        Ok(Self {
            child,
            stdin,
            stdout,
            stdout_reader: Some(stdout_reader),
            next_id: 1,
            response_timeout,
        })
    }

    pub fn run_agent(
        &mut self,
        goal: &str,
        model: Option<&str>,
    ) -> Result<PythonSidecarRunResult, String> {
        let value = self.request(SIDECAR_METHOD_RUN, sidecar_run_params(goal, model))?;
        serde_json::from_value(value)
            .map_err(|error| format!("invalid sidecar run result: {error}"))
    }

    pub fn resume_agent(
        &mut self,
        session_id: &str,
        goal: &str,
        model: Option<&str>,
    ) -> Result<PythonSidecarRunResult, String> {
        let mut params = sidecar_run_params(goal, model);
        if let Some(object) = params.as_object_mut() {
            object.insert("sessionId".to_string(), json!(session_id));
        }
        let value = self.request(SIDECAR_METHOD_RESUME, params)?;
        serde_json::from_value(value)
            .map_err(|error| format!("invalid sidecar run result: {error}"))
    }

    pub fn cancel(&mut self, run_id: &str) -> Result<PythonSidecarStatus, String> {
        let value = self.request(SIDECAR_METHOD_CANCEL, json!({"runId": run_id}))?;
        serde_json::from_value(value)
            .map_err(|error| format!("invalid sidecar cancel result: {error}"))
    }

    pub fn status(&mut self, run_id: &str) -> Result<PythonSidecarStatus, String> {
        let value = self.request(SIDECAR_METHOD_STATUS, json!({"runId": run_id}))?;
        serde_json::from_value(value)
            .map_err(|error| format!("invalid sidecar status result: {error}"))
    }

    pub fn health(&mut self) -> Result<Value, String> {
        self.request(SIDECAR_METHOD_HEALTH, json!({}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_request_id();
        let message = json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}")
            .map_err(|error| format!("failed to write Python sidecar request: {error}"))?;
        self.stdin
            .flush()
            .map_err(|error| format!("failed to flush Python sidecar request: {error}"))?;
        let response = self.read_response()?;
        if response.get("id").and_then(Value::as_i64) != Some(id) {
            return Err("Python sidecar returned mismatched response id".to_string());
        }
        if let Some(error) = response.get("error") {
            let message = error["message"].as_str().unwrap_or("Python sidecar error");
            return Err(message.to_string());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "Python sidecar response missing result".to_string())
    }

    fn read_response(&mut self) -> Result<Value, String> {
        let line = match self.stdout.recv_timeout(self.response_timeout) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {
                self.terminate_child();
                return Err("timed out waiting for Python sidecar response".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(status) = self
                    .child
                    .try_wait()
                    .map_err(|error| format!("failed to poll Python sidecar status: {error}"))?
                {
                    return Err(format!("Python sidecar exited before response: {status}"));
                }
                return Err("Python sidecar closed stdout".to_string());
            }
        };
        serde_json::from_str(line.trim())
            .map_err(|error| format!("invalid Python sidecar JSON: {error}"))
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn terminate_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PythonSidecarClient {
    fn drop(&mut self) {
        self.terminate_child();
        if let Some(stdout_reader) = self.stdout_reader.take() {
            let _ = stdout_reader.join();
        }
    }
}

fn spawn_stdout_reader(stdout: ChildStdout) -> (Receiver<Result<String, String>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(format!(
                        "failed to read Python sidecar response: {error}"
                    )));
                    break;
                }
            }
        }
    });
    (receiver, handle)
}

pub fn sidecar_trace_summary(bridge: &AgentLoopStatusBridge) -> Value {
    json!({
        "component": SIDECAR_COMPONENT,
        "status": bridge.status.as_str(),
        "run_id": bridge.run_id,
        "session_id": bridge.session_id,
        "task_id": bridge.task_id,
        "model_turns": bridge.model_turns,
        "tool_calls": bridge.tool_calls,
        "approval_count": bridge.approval_count,
        "trace_path": bridge.trace_path,
    })
}

fn sidecar_run_params(goal: &str, model: Option<&str>) -> Value {
    let mut params = json!({"goal": goal});
    if let Some(model) = model {
        if let Some(object) = params.as_object_mut() {
            object.insert("model".to_string(), json!(model));
        }
    }
    params
}

fn model_turn_request(
    loop_tools: &ToolBroker,
    input: &AgentLoopInput,
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
        budget: Default::default(),
        context_metadata: json!({}),
        policy_metadata: json!({}),
        trace_metadata: json!({}),
    }
}

fn model_tool_schemas(loop_tools: &ToolBroker) -> Vec<ModelToolSchema> {
    loop_tools
        .model_visible_tools()
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

fn model_messages_from_context(items: &[AgentContextItem]) -> Vec<ModelMessage> {
    assemble_context_items(items, 128_000)
        .messages
        .into_iter()
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

fn observation_message(observation: &ToolObservation) -> ModelMessage {
    let mut message =
        ModelMessage::text(ModelRole::Tool, observation.to_model_payload().to_string());
    message.tool_call_id = Some(observation.tool_call_id.clone());
    message.name = Some(observation.tool_name.clone());
    message
}

fn approval_request(
    input: &AgentLoopInput,
    approval_request_id: &str,
    action: &str,
    reason: &str,
) -> ApprovalRequest {
    let mut request = ApprovalRequest::new(
        approval_request_id,
        input.session_id.clone(),
        input.task_id.clone(),
        action,
    );
    request.reason = reason.to_string();
    request
}

fn permission_operation_for_tool(tool_name: &str) -> PermissionOperation {
    match tool_name {
        TOOL_EDIT | TOOL_PATCH => PermissionOperation::Write,
        _ => PermissionOperation::Read,
    }
}

fn permission_resource_for_tool(call: &ModelToolCall) -> String {
    path_argument(&call.arguments).unwrap_or_else(|| call.tool_name.clone())
}

fn tool_call_targets_sensitive_resource(call: &ModelToolCall) -> bool {
    path_argument(&call.arguments)
        .map(|path| is_sensitive_tool_path(&path))
        .unwrap_or(false)
}

fn is_sensitive_tool_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.split('/').any(|component| {
        component == ".env"
            || component.strip_prefix(".env.").is_some()
            || component == ".ssh"
            || component == ".git"
            || component == "credentials"
            || component == "credentials.json"
            || component.contains("secret")
            || component.ends_with(".key")
            || component.ends_with(".pem")
            || component.ends_with(".p12")
            || component.ends_with(".pfx")
    })
}

fn path_argument(arguments: &Value) -> Option<String> {
    arguments
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
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

fn invalid_tool_arguments(error: serde_json::Error) -> AgentLoopToolError {
    AgentLoopToolError::InvalidArguments(error.to_string())
}

fn workspace_tool_failure(error: AgentLoopToolError) -> ToolResult {
    let error_code = match &error {
        AgentLoopToolError::InvalidArguments(_) => "invalid_tool_arguments",
        AgentLoopToolError::Workspace(WorkspaceToolError::OutsideWorkspace(_)) => {
            "outside_workspace"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::ProtectedPath(_)) => "protected_path",
        AgentLoopToolError::Workspace(WorkspaceToolError::BinaryPattern) => "binary_pattern",
        AgentLoopToolError::Workspace(WorkspaceToolError::ReadFailed(_)) => "tool_read_failed",
        AgentLoopToolError::Workspace(WorkspaceToolError::ExpectedContentMissing(_)) => {
            "expected_content_missing"
        }
        AgentLoopToolError::Workspace(WorkspaceToolError::InvalidInput(_)) => "invalid_tool_input",
    };
    ToolResult::failure(error_code, json!({"summary": error.to_string()}))
}

fn action_matches(action: &Value, kind: &str, status: &str) -> bool {
    action.get("kind").and_then(Value::as_str) == Some(kind)
        && action.get("status").and_then(Value::as_str) == Some(status)
}
