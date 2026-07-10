#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, ModelMessage, ModelPreferences, ModelPurpose, ModelRole,
    ModelToolCall, ModelToolParseStatus, ModelToolSchema, ModelTurnRequest, ModelTurnStatus,
    Provider, provider_error_response,
};
use singularity_policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionDecision,
    PermissionDecisionOutcome, PermissionOperation, PermissionRequest, PolicyEngine,
};
#[cfg(windows)]
use singularity_tools::{
    CommandExecutionStatus, CommandRequest, CommandSemanticStatus, SandboxBackend,
    SandboxFilesystemMode, WindowsRestrictedTokenSandboxBackend,
};
use singularity_tools::{
    CommandToolInput, EditToolInput, GrepToolInput, ListToolInput, ReadToolInput, ToolBroker,
    ToolBrokerDecision, ToolCallRequest, ToolOutput, ToolResult, WorkspacePatch,
    WorkspaceToolError, WorkspaceTools, command_scope_digest, command_scope_resource,
    is_protected_path,
};
use thiserror::Error;

#[cfg(not(windows))]
const STRICT_COMMAND_SANDBOX_UNSUPPORTED_PLATFORM: &str =
    "strict_command_sandbox_unsupported_platform";
#[cfg(windows)]
const STRICT_COMMAND_SANDBOX_PROBE_FAILED: &str = "strict_command_sandbox_probe_failed";
#[cfg(windows)]
const NATIVE_AGENT_LOOP_READY_REASON: &str =
    "native Rust AgentLoop is available as the default runtime";
#[cfg(windows)]
const NATIVE_AGENT_LOOP_SANDBOX_BLOCKED_REASON: &str =
    "native Rust AgentLoop requires a working Windows restricted-token command sandbox";
#[cfg(not(windows))]
const NATIVE_AGENT_LOOP_UNSUPPORTED_PLATFORM_REASON: &str =
    "native Rust AgentLoop requires the Windows restricted-token command sandbox";
#[cfg(windows)]
const AGENT_LOOP_CAPABILITY_PROBE_TIMEOUT_SECONDS: u64 = 5;
const DEFAULT_MAX_AGENT_LOOP_TURNS: u32 = 4;
const EMPTY_FINAL_ANSWER_ERROR: &str = "empty final answer";
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
#[cfg(windows)]
static AGENT_LOOP_CAPABILITY_CACHE: OnceLock<AgentLoopCapability> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    NotMigrated,
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

impl From<&str> for AgentStatus {
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
    pub error: Option<String>,
}

impl AgentRunStatus {
    pub fn not_migrated() -> Self {
        Self {
            status: AgentStatus::NotMigrated,
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
            error: None,
        }
    }

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
            AGENT_LOOP_CAPABILITY_CACHE
                .get_or_init(windows_agent_loop_capability)
                .clone()
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

#[cfg(windows)]
fn windows_agent_loop_capability() -> AgentLoopCapability {
    match probe_windows_command_sandbox() {
        Ok(()) => AgentLoopCapability {
            available: true,
            status: AgentStatus::Completed,
            reason: NATIVE_AGENT_LOOP_READY_REASON.to_string(),
            blockers: Vec::new(),
        },
        Err(blocker) => AgentLoopCapability {
            available: false,
            status: AgentStatus::Blocked,
            reason: NATIVE_AGENT_LOOP_SANDBOX_BLOCKED_REASON.to_string(),
            blockers: vec![blocker],
        },
    }
}

#[cfg(windows)]
fn probe_windows_command_sandbox() -> Result<(), String> {
    let workspace = create_sandbox_probe_workspace()?;
    let workspace_display = workspace.to_string_lossy().to_string();
    let mut request = CommandRequest::project_verification(
        "agent_loop_capability_probe",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo singularity-sandbox-ready".to_string(),
        ],
        workspace_display.clone(),
        workspace_display,
    );
    request.filesystem.mode = SandboxFilesystemMode::ReadOnly;
    request.timeout_seconds = AGENT_LOOP_CAPABILITY_PROBE_TIMEOUT_SECONDS;
    let result = WindowsRestrictedTokenSandboxBackend::new().execute(&request);
    let _ = std::fs::remove_dir_all(&workspace);
    if result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
    {
        Ok(())
    } else {
        Err(format!(
            "{STRICT_COMMAND_SANDBOX_PROBE_FAILED}:{:?}:{:?}",
            result.execution_status, result.semantic_status
        ))
    }
}

#[cfg(windows)]
fn create_sandbox_probe_workspace() -> Result<PathBuf, String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "singularity-agentloop-sandbox-probe-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&path).map_err(|error| {
        format!("{STRICT_COMMAND_SANDBOX_PROBE_FAILED}:workspace_unavailable:{error}")
    })?;
    Ok(path)
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
            provider_metadata: json!({}),
        })
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
        if input.interrupted {
            return AgentLoopResult {
                status: AgentStatus::Cancelled,
                completed: false,
                final_answer: None,
                model_turns: 0,
                tool_calls: 0,
                approval_count: 0,
                approval_requests: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_results: Vec::new(),
                tool_repairs: Vec::new(),
                error: None,
            };
        }
        let mut tool_results = Vec::new();
        let mut tool_repairs = Vec::new();
        let mut approval_requests = Vec::new();
        let mut pending_tool_calls = Vec::new();
        let mut used_approval_grants = HashSet::new();
        let context = assemble_context_items(&input.input, DEFAULT_MAX_CONTEXT_TOKENS);
        let mut messages = model_messages_from_context(&context);
        let max_turns = input.max_turns.max(1);
        for turn_index in 0..max_turns {
            let request = model_turn_request(
                &self.tool_broker,
                input,
                &context,
                turn_index,
                messages.clone(),
            );
            let response = match self.provider.complete(&request) {
                Ok(response) => response,
                Err(error) => provider_error_response(&request, error),
            };
            if response.status != ModelTurnStatus::Success {
                return AgentLoopResult {
                    status: AgentStatus::Failed,
                    completed: false,
                    final_answer: None,
                    model_turns: turn_index + 1,
                    tool_calls: tool_results.len() as u32,
                    approval_count: 0,
                    approval_requests,
                    pending_tool_calls,
                    tool_results,
                    tool_repairs,
                    error: response.error.map(|error| error.message),
                };
            }
            if response.tool_calls.is_empty() {
                let final_answer = assistant_message_text(response.assistant_message.as_ref());
                if final_answer.trim().is_empty() {
                    return AgentLoopResult {
                        status: AgentStatus::Failed,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: tool_results.len() as u32,
                        approval_count: 0,
                        approval_requests,
                        pending_tool_calls,
                        tool_results,
                        tool_repairs,
                        error: Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                    };
                }
                return AgentLoopResult {
                    status: AgentStatus::Completed,
                    completed: true,
                    final_answer: Some(final_answer),
                    model_turns: turn_index + 1,
                    tool_calls: tool_results.len() as u32,
                    approval_count: 0,
                    approval_requests,
                    pending_tool_calls,
                    tool_results,
                    tool_repairs,
                    error: None,
                };
            }
            let assistant_tool_message = response
                .assistant_message
                .as_ref()
                .filter(|message| !message.tool_calls.is_empty())
                .cloned()
                .unwrap_or_else(|| ModelMessage::assistant_tool_calls(response.tool_calls.clone()));
            messages.push(assistant_tool_message);
            for call in &response.tool_calls {
                if input.interrupted {
                    return AgentLoopResult {
                        status: AgentStatus::Cancelled,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: tool_results.len() as u32,
                        approval_count: approval_requests.len() as u32,
                        approval_requests,
                        pending_tool_calls,
                        tool_results,
                        tool_repairs,
                        error: None,
                    };
                }
                let decision = self.tool_decision(input, call, &mut used_approval_grants);
                if let ToolBrokerDecision::Ask {
                    approval_request_id,
                    reason,
                } = &decision
                {
                    approval_requests.push(approval_request(
                        input,
                        approval_request_id,
                        call,
                        reason,
                    ));
                    pending_tool_calls.push(PendingToolCall::new(input, call));
                    let tool_result = self.execute_tool(input, call, decision);
                    tool_results.push(tool_result);
                    return AgentLoopResult {
                        status: AgentStatus::Blocked,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: tool_results.len() as u32,
                        approval_count: approval_requests.len() as u32,
                        approval_requests,
                        pending_tool_calls,
                        tool_results,
                        tool_repairs,
                        error: None,
                    };
                }
                let tool_result = self.execute_tool(input, call, decision);
                let failed_tool_result = !tool_result.ok;
                tool_results.push(tool_result.clone());
                messages.push(tool_result_message(&tool_result));
                if failed_tool_result {
                    if is_repairable_tool_result(&tool_result) {
                        tool_repairs.push(tool_repair(input, turn_index, &tool_result));
                        break;
                    }
                    let error_code = tool_result
                        .error_code
                        .as_deref()
                        .unwrap_or("tool_execution_failed");
                    return AgentLoopResult {
                        status: AgentStatus::Failed,
                        completed: false,
                        final_answer: None,
                        model_turns: turn_index + 1,
                        tool_calls: tool_results.len() as u32,
                        approval_count: 0,
                        approval_requests,
                        pending_tool_calls,
                        tool_results,
                        tool_repairs,
                        error: Some(format!("tool execution failed: {error_code}")),
                    };
                }
            }
        }
        AgentLoopResult {
            status: AgentStatus::Failed,
            completed: false,
            final_answer: None,
            model_turns: max_turns,
            tool_calls: tool_results.len() as u32,
            approval_count: 0,
            approval_requests,
            pending_tool_calls,
            tool_results,
            tool_repairs,
            error: Some("max turns exceeded".to_string()),
        }
    }

    pub fn resume_pending_tool_call(
        &self,
        input: &AgentLoopInput,
        pending: &PendingToolCall,
    ) -> AgentLoopResult {
        if input.interrupted {
            return AgentLoopResult {
                status: AgentStatus::Cancelled,
                completed: false,
                final_answer: None,
                model_turns: 0,
                tool_calls: 0,
                approval_count: 0,
                approval_requests: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_results: Vec::new(),
                tool_repairs: Vec::new(),
                error: None,
            };
        }
        let call = match pending.to_model_tool_call() {
            Ok(call) => call,
            Err(error) => {
                return AgentLoopResult {
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
                    error: Some(format!("invalid pending tool call arguments: {error}")),
                };
            }
        };
        let context = assemble_context_items(&input.input, DEFAULT_MAX_CONTEXT_TOKENS);
        let mut messages = model_messages_from_context(&context);
        messages.push(ModelMessage::assistant_tool_calls(vec![call.clone()]));
        let mut used_approval_grants = HashSet::new();
        let decision = self.tool_decision(input, &call, &mut used_approval_grants);
        if !matches!(decision, ToolBrokerDecision::Approved { .. }) {
            return AgentLoopResult {
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
                error: Some("pending tool call approval did not match".to_string()),
            };
        }
        let tool_result = self.execute_tool(input, &call, decision);
        let failed_tool_result = !tool_result.ok;
        messages.push(tool_result_message(&tool_result));
        let tool_results = vec![tool_result.clone()];
        let mut tool_repairs = Vec::new();
        if failed_tool_result {
            if is_repairable_tool_result(&tool_result) {
                tool_repairs.push(tool_repair(input, 0, &tool_result));
            }
            let error_code = tool_result
                .error_code
                .as_deref()
                .unwrap_or("tool_execution_failed");
            return AgentLoopResult {
                status: AgentStatus::Failed,
                completed: false,
                final_answer: None,
                model_turns: 0,
                tool_calls: tool_results.len() as u32,
                approval_count: 1,
                approval_requests: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_results,
                tool_repairs,
                error: Some(format!("tool execution failed: {error_code}")),
            };
        }
        let turn_index = 0;
        let request = model_turn_request(&self.tool_broker, input, &context, turn_index, messages);
        let response = match self.provider.complete(&request) {
            Ok(response) => response,
            Err(error) => provider_error_response(&request, error),
        };
        if response.status != ModelTurnStatus::Success {
            return AgentLoopResult {
                status: AgentStatus::Failed,
                completed: false,
                final_answer: None,
                model_turns: turn_index + 1,
                tool_calls: tool_results.len() as u32,
                approval_count: 1,
                approval_requests: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_results,
                tool_repairs,
                error: response.error.map(|error| error.message),
            };
        }
        if response.tool_calls.is_empty() {
            let final_answer = assistant_message_text(response.assistant_message.as_ref());
            if final_answer.trim().is_empty() {
                return AgentLoopResult {
                    status: AgentStatus::Failed,
                    completed: false,
                    final_answer: None,
                    model_turns: turn_index + 1,
                    tool_calls: tool_results.len() as u32,
                    approval_count: 1,
                    approval_requests: Vec::new(),
                    pending_tool_calls: Vec::new(),
                    tool_results,
                    tool_repairs,
                    error: Some(EMPTY_FINAL_ANSWER_ERROR.to_string()),
                };
            }
            return AgentLoopResult {
                status: AgentStatus::Completed,
                completed: true,
                final_answer: Some(final_answer),
                model_turns: turn_index + 1,
                tool_calls: tool_results.len() as u32,
                approval_count: 1,
                approval_requests: Vec::new(),
                pending_tool_calls: Vec::new(),
                tool_results,
                tool_repairs,
                error: None,
            };
        }
        AgentLoopResult {
            status: AgentStatus::Failed,
            completed: false,
            final_answer: None,
            model_turns: turn_index + 1,
            tool_calls: tool_results.len() as u32,
            approval_count: 1,
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_results,
            tool_repairs,
            error: Some("tool call after approval resume is not supported".to_string()),
        }
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
        let operation = permission_operation_for_tool(&call.tool_name);
        let resources = permission_resources_for_tool(call);
        let mut first_allow = None;
        let mut first_ask = None;
        for resource in resources {
            let mut request =
                PermissionRequest::new(call.tool_name.clone(), operation, resource.clone());
            if is_protected_path(&resource) {
                request = request.with_sensitive_resource();
            }
            let decision = self.policy.evaluate(&request);
            match decision.outcome {
                PermissionDecisionOutcome::Deny => return decision,
                PermissionDecisionOutcome::Ask if first_ask.is_none() => first_ask = Some(decision),
                PermissionDecisionOutcome::Allow if first_allow.is_none() => {
                    first_allow = Some(decision);
                }
                _ => {}
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
                    .command(input.clone())
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
    pub public: bool,
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
            public: true,
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

pub fn assemble_context_items(items: &[AgentContextItem], max_tokens: u32) -> ContextBundle {
    let mut candidates: Vec<(usize, &AgentContextItem)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.public && !item.evaluator_only)
        .collect();
    candidates.sort_by_key(|(index, item)| (item.priority.rank(), *index));

    let mut used_tokens = 0;
    let mut included_item_ids = Vec::new();
    let mut excluded_item_ids: Vec<String> = items
        .iter()
        .filter(|item| !item.public || item.evaluator_only)
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

pub fn planner_next_action(state: &PlannerState) -> PlannerNextAction {
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

pub fn repair_next_action(repair: &ToolRepair) -> RepairNextAction {
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
    status: AgentStatus,
    final_answer: &str,
) -> FinalReportMapping {
    let run_status = match status {
        AgentStatus::Completed => "completed",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Cancelled | AgentStatus::CancelRequested => "interrupted",
        AgentStatus::Failed => "failed",
        AgentStatus::Running | AgentStatus::NotMigrated => "running",
    };
    FinalReportMapping {
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
        contract_satisfaction: json!({"satisfied": status == AgentStatus::Completed}),
        created_at: String::new(),
        metadata: json!({}),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlannerState {
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
pub struct ContextSummaryEnvelope {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FinalReportMapping {
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
        budget: Default::default(),
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
    let resource =
        command_scope_resource(&input.argv, &input.sandbox_mode(), &input.network_access());
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
        "sandbox_mode": input.sandbox_mode(),
        "network_access": input.network_access(),
        "sandbox_backend": "unavailable",
        "sandbox_enforcement": "strict",
        "command_scope_digest": command_scope_digest(
            &input.argv,
            &input.sandbox_mode(),
            &input.network_access(),
        ),
        "command_provenance": "agent_requested",
    });
    output
}

fn action_matches(action: &Value, kind: &str, status: &str) -> bool {
    action.get("kind").and_then(Value::as_str) == Some(kind)
        && action.get("status").and_then(Value::as_str) == Some(status)
}
