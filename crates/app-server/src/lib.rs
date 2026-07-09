#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopCapability, AgentLoopInput, AgentRunStatus, AgentStatus, ApprovalGrant,
    PendingToolCall,
};
use singularity_core::{ErrorCode, contains_sensitive_text};
use singularity_model::{OpenAiProvider, Provider};
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, PermissionDecisionOutcome,
    PermissionOperation, PermissionProfile, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{
    AgentCapabilityResult, AppEvent, ApprovalCenterResult, ApprovalListResult, ArtifactFetchParams,
    ArtifactFetchResult, EvalRunParams, EvalRunResult, EventSubscribeParams, EventSubscribeResult,
    InitializeParams, InitializeResult, JsonRpcMessage, Method, ServerCapabilitiesResult, Thread,
    ThreadDeleteResult, ThreadForkParams, ThreadForkResult, ThreadIdParams, ThreadListResult,
    ThreadResult, ThreadStartParams, ThreadStartResult, TraceEvent, TraceListParams,
    TraceListResult, TraceShowParams, TraceTailParams, TransportCapability, Turn, TurnIdParams,
    TurnInterruptResult, TurnResult, TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_store::{SessionStore, StoreError};
use singularity_tools::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus, CommandToolInput,
    SandboxBackend, SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolRegistry, ToolSpec,
    WindowsRestrictedTokenSandboxBackend, WorkspaceTools, command_scope_digest,
    command_scope_resource,
};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const TRACE_RUN_NOT_FOUND: &str = "Trace run not found";
const TRACE_EVENT_NOT_FOUND: &str = "Trace event not found";
const PENDING_APPROVAL_NOT_FOUND: &str = "Pending approval not found";
const APPROVAL_REQUEST_INTERNAL_ONLY: &str =
    "approval/request is internal to the Rust AgentLoop approval ledger";
const ARTIFACT_NOT_FOUND: &str = "Artifact not found";
const EVENT_SUBSCRIPTION_ID: &str = "subscription_app_server_events";
const TOOL_READ: &str = "builtin.read";
const TOOL_LIST: &str = "builtin.list";
const TOOL_GREP: &str = "builtin.grep";
const TOOL_EDIT: &str = "builtin.edit";
const TOOL_PATCH: &str = "builtin.patch";
const TOOL_COMMAND: &str = "builtin.command";
const EVAL_TASK_SET_SCHEMA: &str = "evaluation.task_set/v1";
const EVAL_RESULT_SCHEMA: &str = "evaluation.result/v1";
const EVAL_OUTPUT_DIR_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const EVAL_PROVIDER_BLOCKER: &str = "provider_config_missing";
const EVAL_WORKSPACE_BLOCKER: &str = "eval_workspace_failed";
const EVAL_CAPABILITY_BLOCKER: &str = "native_agent_loop_unavailable";
const EVAL_AGENT_BLOCKER: &str = "agent_loop_failed";
const EVAL_VERIFICATION_BLOCKER: &str = "verification_failed";
const EVAL_RUNNER_NAME: &str = "rust_native";
const EVAL_REPO_DIR: &str = "repo";
const EVAL_TEST_PATCH_FILE: &str = "eval-test.patch";
const EVAL_DEFAULT_MAX_TURNS: u32 = 24;
const EVAL_DEFAULT_TIMEOUT_SECONDS: u64 = 300;
const EVAL_PREPARE_TIMEOUT_SECONDS: u64 = 900;
const EVAL_GIT_TIMEOUT_SECONDS: u64 = 900;
static EVAL_COMMAND_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

pub type AppServerResult<T> = Result<T, AppServerError>;

pub struct AppServer {
    store: SessionStore,
    initialized: bool,
    initialized_acknowledged: bool,
    event_filter: Option<Vec<String>>,
    shutdown_requested: bool,
}

impl AppServer {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
            event_filter: None,
            shutdown_requested: false,
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    pub fn handle_json(&mut self, line: &str) -> AppServerResult<Vec<Value>> {
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        self.handle(message)
    }

    pub fn handle(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let id = message.id.clone();
        let Some(method_name) = message.method.as_deref() else {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::invalid_request("Missing method"))
                    .to_wire_value(),
            ]);
        };
        let Some(method) = Method::parse(method_name) else {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::method_not_found(method_name)).to_wire_value(),
            ]);
        };

        if matches!(method, Method::Initialized) && !self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::not_initialized()).to_wire_value(),
            ]);
        }
        if !matches!(method, Method::Initialize | Method::Initialized)
            && !self.initialized_acknowledged
        {
            return Ok(vec![
                JsonRpcMessage::error(id, ErrorCode::not_initialized()).to_wire_value(),
            ]);
        }

        match method {
            Method::Initialize => self.initialize(message),
            Method::Initialized => {
                self.initialized_acknowledged = true;
                Ok(Vec::new())
            }
            Method::ServerCapabilities => self.server_capabilities(message),
            Method::ThreadList => self.thread_list(message),
            Method::ThreadRead | Method::ThreadResume => self.thread_read(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadFork => self.thread_fork(message),
            Method::ThreadArchive => self.thread_archive(message),
            Method::ThreadDelete => self.thread_delete(message),
            Method::TurnStart => self.turn_start(message),
            Method::EvalRun => self.eval_run(message),
            Method::AgentCapability => self.agent_capability(message),
            Method::TurnInterrupt => self.turn_interrupt(message),
            Method::TurnStatus => self.turn_status(message),
            Method::ApprovalList => self.approval_list(message),
            Method::ApprovalCenter => self.approval_center(message),
            Method::ApprovalRequest => self.approval_request(message),
            Method::ApprovalDecision => self.approval_decision(message),
            Method::EventSubscribe => self.event_subscribe(message),
            Method::ArtifactFetch => self.artifact_fetch(message),
            Method::TraceList => self.trace_list(message),
            Method::TraceShow => self.trace_show(message),
            Method::TraceTail => self.trace_tail(message),
            Method::ServerShutdown => self.server_shutdown(message),
        }
    }

    fn initialize(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        if self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(message.id, ErrorCode::already_initialized()).to_wire_value(),
            ]);
        }
        let _params: InitializeParams = message.params_as()?;
        self.initialized = true;
        Ok(vec![
            JsonRpcMessage::response(message.id, serde_json::to_value(InitializeResult::local())?)
                .to_wire_value(),
        ])
    }

    fn server_capabilities(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            ServerCapabilitiesResult {
                transports: vec![
                    TransportCapability {
                        transport: "stdio".to_string(),
                        available: true,
                        auth_token_required: false,
                    },
                    TransportCapability {
                        transport: "websocket".to_string(),
                        available: false,
                        auth_token_required: true,
                    },
                ],
            },
        )
    }

    fn thread_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let threads = self.store.list_threads()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    fn thread_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.get_thread(&params.thread_id) {
            Ok(thread) => json_response(message.id, ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = message.params_as()?;
        let (thread, _trace) = self.store.create_thread_with_trace(
            params.model.as_deref(),
            params.cwd.as_deref(),
            "app_server",
            "thread started",
        )?;
        let mut messages = Vec::new();
        messages.extend(self.event_notification(AppEvent::thread_started(&thread)));
        messages.push(
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadStartResult {
                    thread: thread.clone(),
                })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    fn thread_fork(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadForkParams = message.params_as()?;
        if let Err(error) = self.store.get_thread(&params.thread_id) {
            return match error {
                StoreError::NotFound(_) => not_found_response(message.id, THREAD_NOT_FOUND),
                other => Err(other.into()),
            };
        }
        let thread = self
            .store
            .create_thread(params.model.as_deref(), params.cwd.as_deref())?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ThreadForkResult {
                    source_thread_id: params.thread_id,
                    thread,
                })?,
            )
            .to_wire_value(),
        ])
    }

    fn thread_archive(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Archived,
        ) {
            Ok(thread) => json_response(message.id, ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn thread_delete(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = message.params_as()?;
        match self.store.delete_thread(&params.thread_id) {
            Ok(()) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(ThreadDeleteResult {
                        thread_id: params.thread_id,
                        deleted: true,
                    })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, THREAD_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnStartParams = match message.params_as() {
            Ok(params) => params,
            Err(error) => {
                return json_error(
                    message.id,
                    ErrorCode::invalid_request(format!("invalid turn/start params: {error}")),
                );
            }
        };
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let capability = AgentLoopCapability::current();
        if !native_capability_ready(&capability) {
            return invalid_request_response(
                message.id,
                native_agent_loop_unavailable_message(&capability),
            );
        }
        let payload = serde_json::to_value(&params.input)?;
        let (turn, _item, _trace) = match self.store.create_turn_with_input_and_trace(
            &params.thread_id,
            AgentStatus::Running.as_str(),
            payload,
            "app_server",
            "turn started",
        ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.id, THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };

        let mut messages = Vec::new();
        messages.extend(self.event_notification(AppEvent::turn_started(&turn)));
        let status = self.run_native_agent_loop(&thread, &params, &turn.turn_id)?;
        let turn = self.update_turn_from_run_status(turn, &status)?;
        self.append_native_trace(&params.thread_id, &turn.turn_id, &status)?;
        messages.extend(self.agent_terminal_item_events(&status, &turn)?);
        messages.push(
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(TurnStartResult { turn: turn.clone() })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    fn agent_capability(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            AgentCapabilityResult {
                native_agent_loop: serde_json::to_value(AgentLoopCapability::current())?,
            },
        )
    }

    fn eval_run(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EvalRunParams = message.params_as()?;
        let manifest_text = match std::fs::read_to_string(&params.manifest) {
            Ok(text) => text,
            Err(error) => {
                return json_error(
                    message.id,
                    ErrorCode::invalid_request(format!("invalid eval manifest: {error}")),
                );
            }
        };
        let manifest: Value = match serde_json::from_str(&manifest_text) {
            Ok(value) => value,
            Err(error) => {
                return json_error(
                    message.id,
                    ErrorCode::invalid_request(format!("invalid eval manifest: {error}")),
                );
            }
        };
        if manifest.get("schema_version").and_then(Value::as_str) != Some(EVAL_TASK_SET_SCHEMA) {
            return json_error(
                message.id,
                ErrorCode::invalid_request("invalid eval manifest: unsupported schema_version"),
            );
        }
        let Some(tasks) = manifest.get("tasks").and_then(Value::as_array) else {
            return json_error(
                message.id,
                ErrorCode::invalid_request("invalid eval manifest: tasks must be an array"),
            );
        };
        if tasks.is_empty() {
            return json_error(
                message.id,
                ErrorCode::invalid_request("invalid eval manifest: tasks must not be empty"),
            );
        }
        let mut result = if native_agent_loop_ready() {
            run_native_eval(&params, tasks)
        } else {
            run_native_eval_blocked_by_capability(&params, tasks)
        };
        if let Err(error) = write_eval_artifacts(&mut result, params.output_root.as_deref()) {
            return json_error(
                message.id,
                ErrorCode::invalid_request(format!("failed to write eval artifacts: {error}")),
            );
        }
        json_response(message.id, result)
    }

    fn run_native_agent_loop(
        &self,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
    ) -> AppServerResult<AgentRunStatus> {
        let provider = match OpenAiProvider::from_env(|name| std::env::var(name).ok()) {
            Ok(provider) => provider,
            Err(error) => {
                return Ok(AgentRunStatus::failed(error.message).with_status(AgentStatus::Failed));
            }
        };
        self.run_native_agent_loop_with_provider(provider, thread, params, turn_id, None)
    }

    fn resume_native_agent_loop(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        if !native_agent_loop_ready() {
            return Ok(None);
        }
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        if pending_tool_call.is_none() {
            return Ok(None);
        }
        let provider = match OpenAiProvider::from_env(|name| std::env::var(name).ok()) {
            Ok(provider) => provider,
            Err(error) => {
                let turn_id = &request.turn_id;
                let turn = self.store.get_turn(turn_id)?;
                let run_status = native_approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    pending_tool_call.as_ref(),
                    AgentStatus::Failed,
                    "unavailable",
                    error.message,
                );
                return Ok(Some((turn, run_status)));
            }
        };
        self.resume_native_agent_loop_after_gate(request, decision, pending_tool_call, provider)
    }

    fn resume_native_agent_loop_after_gate<P>(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        provider: P,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>>
    where
        P: Provider,
    {
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        let turn_id = &request.turn_id;
        let turn = self.store.get_turn(turn_id)?;
        if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            return Ok(None);
        }
        let Some(pending_tool_call) = pending_tool_call else {
            return Ok(None);
        };
        let pending = match serde_json::from_value::<PendingToolCall>(pending_tool_call.clone()) {
            Ok(pending) => pending,
            Err(error) => {
                let run_status = native_approval_terminal_status(
                    &turn,
                    request,
                    decision,
                    Some(&pending_tool_call),
                    AgentStatus::Failed,
                    "unavailable",
                    format!("invalid pending tool call: {error}"),
                );
                return Ok(Some((turn, run_status)));
            }
        };
        if pending.request_id != request.request_id {
            let run_status = native_approval_terminal_status(
                &turn,
                request,
                decision,
                Some(&pending_tool_call),
                AgentStatus::Failed,
                "unavailable",
                "pending tool call request mismatch",
            );
            return Ok(Some((turn, run_status)));
        }
        let thread = self.store.get_thread(&turn.thread_id)?;
        let user_input = self.store.get_turn_user_input(turn_id)?;
        let params = TurnStartParams {
            thread_id: turn.thread_id.clone(),
            input: serde_json::from_value(user_input)?,
        };
        let grant = ApprovalGrant::allow(
            request.request_id.clone(),
            request.action.clone(),
            request.resources.clone(),
        );
        let registry = native_workspace_registry();
        let workspace_root = native_workspace_root(&thread);
        let workspace_root_display = workspace_root.to_string_lossy().into_owned();
        let policy = native_workspace_policy(workspace_root_display);
        let loop_input = native_loop_input(&thread, &params, turn_id).with_approval_grant(grant);
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(native_workspace_tools(workspace_root))
            .resume_pending_tool_call(&loop_input, &pending);
        let mut run_status = result.to_run_status(&loop_input);
        if run_status.audit_events.is_empty() && pending.tool_name == TOOL_COMMAND {
            let audit_status = native_approval_terminal_status(
                &turn,
                request,
                decision,
                Some(&pending_tool_call),
                run_status.status.clone(),
                "unavailable",
                run_status
                    .error
                    .clone()
                    .unwrap_or_else(|| "approval resume did not execute command".to_string()),
            );
            run_status.audit_events = audit_status.audit_events;
        }
        Ok(Some((turn, run_status)))
    }

    fn run_native_agent_loop_with_provider<P>(
        &self,
        provider: P,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        approval_grant: Option<ApprovalGrant>,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider,
    {
        let registry = native_workspace_registry();
        let workspace_root = native_workspace_root(thread);
        let workspace_root_display = workspace_root.to_string_lossy().into_owned();
        let policy = native_workspace_policy(workspace_root_display);
        let mut loop_input = native_loop_input(thread, params, turn_id);
        if let Some(grant) = approval_grant {
            loop_input = loop_input.with_approval_grant(grant);
        }
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(native_workspace_tools(workspace_root))
            .run(&loop_input);
        let pending_by_request = result
            .pending_tool_calls
            .iter()
            .map(|pending| (pending.request_id.as_str(), pending))
            .collect::<HashMap<_, _>>();
        for request in &result.approval_requests {
            let pending_tool_call = pending_by_request
                .get(request.request_id.as_str())
                .map(|pending| serde_json::to_value(*pending))
                .transpose()?;
            match self.store.create_approval_with_pending_tool_call_and_trace(
                request,
                pending_tool_call,
                "approval",
                "approval requested",
            ) {
                Ok(_) => {}
                Err(StoreError::AlreadyExists(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(result.to_run_status(&loop_input))
    }

    fn update_turn_from_run_status(
        &self,
        turn: Turn,
        run_status: &AgentRunStatus,
    ) -> AppServerResult<Turn> {
        let status = match run_status.status {
            singularity_agent::AgentStatus::Completed => Some(TurnStatus::Completed),
            singularity_agent::AgentStatus::Blocked => Some(TurnStatus::Blocked),
            singularity_agent::AgentStatus::Failed => Some(TurnStatus::Failed),
            singularity_agent::AgentStatus::CancelRequested => Some(TurnStatus::Interrupted),
            singularity_agent::AgentStatus::Cancelled => Some(TurnStatus::Interrupted),
            singularity_agent::AgentStatus::Running => Some(TurnStatus::Running),
            singularity_agent::AgentStatus::NotMigrated => None,
        };
        if let Some(status) = status {
            return Ok(self.store.update_turn_state(
                &turn.turn_id,
                status,
                run_status.status.as_str(),
            )?);
        }
        Ok(turn)
    }

    fn append_native_trace(
        &self,
        thread_id: &str,
        turn_id: &str,
        run_status: &AgentRunStatus,
    ) -> AppServerResult<()> {
        let mut event = TraceEvent::new(
            format!("trace_{turn_id}_native_agent_loop"),
            thread_id,
            turn_id,
            "agent_loop",
            "Rust native AgentLoop result translated",
        );
        event.payload = json!({
            "component": "agent_loop",
            "status": run_status.status.as_str(),
            "run_id": run_status.run_id,
            "session_id": run_status.session_id,
            "task_id": run_status.task_id,
            "model_turns": run_status.model_turns,
            "tool_calls": run_status.tool_calls,
            "approval_count": run_status.approval_count,
            "audit_events": run_status.audit_events,
            "error": run_status.error.as_deref().map(redact_app_server_text),
        });
        self.store.append_trace(&event)?;
        Ok(())
    }

    fn turn_interrupt(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) if is_terminal_turn_status(&turn.status) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TurnInterruptResult {
                        status: turn_status_str(&turn.status).to_string(),
                        turn_id: turn.turn_id,
                        agent_loop_status: Some(turn.agent_loop_status),
                    })?,
                )
                .to_wire_value(),
            ]),
            Ok(turn) => {
                let thread_id = turn.thread_id.clone();
                let turn = self.store.update_turn_state(
                    &turn.turn_id,
                    TurnStatus::Interrupted,
                    "cancel_requested",
                )?;
                let trace = TraceEvent {
                    payload: json!({
                        "turn_id": turn.turn_id,
                        "agent_loop_status": turn.agent_loop_status,
                    }),
                    ..TraceEvent::new(
                        format!("trace_{}_interrupt_requested", turn.turn_id),
                        thread_id,
                        turn.turn_id.clone(),
                        "app_server",
                        "turn interrupt requested",
                    )
                };
                self.store.append_trace(&trace)?;
                Ok(vec![
                    JsonRpcMessage::response(
                        message.id,
                        serde_json::to_value(TurnInterruptResult {
                            turn_id: turn.turn_id,
                            status: "interrupted".to_string(),
                            agent_loop_status: Some(turn.agent_loop_status),
                        })?,
                    )
                    .to_wire_value(),
                ])
            }
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn turn_status(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = message.params_as()?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(message.id, TurnResult { turn }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TURN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn server_shutdown(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.shutdown_requested = true;
        Ok(vec![
            JsonRpcMessage::response(message.id, json!({"shutdown": true})).to_wire_value(),
        ])
    }

    fn approval_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let approvals = self.store.list_pending_approvals()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.id,
                serde_json::to_value(ApprovalListResult { approvals })?,
            )
            .to_wire_value(),
        ])
    }

    fn approval_center(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        json_response(
            message.id,
            ApprovalCenterResult {
                pending_approvals: self.store.list_pending_approvals()?,
                decisions: self.store.list_approval_decisions()?,
            },
        )
    }

    fn approval_request(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let _request: ApprovalRequest = message.params_as()?;
        invalid_request_response(message.id, APPROVAL_REQUEST_INTERNAL_ONLY)
    }

    fn approval_decision(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = message.params_as()?;
        let recorded = match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(recorded) => recorded,
            Err(error) => {
                return match error {
                    StoreError::NotFound(_) => {
                        not_found_response(message.id, PENDING_APPROVAL_NOT_FOUND)
                    }
                    other => Err(other.into()),
                };
            }
        };
        let pending_tool_call = recorded.pending_tool_call.clone();
        let resumed =
            self.resume_native_agent_loop(&recorded.request, &decision, pending_tool_call.clone())?;
        let mut messages = Vec::new();
        let terminal = if let Some(resumed) = resumed {
            Some(resumed)
        } else {
            self.native_approval_no_resume_status(
                &recorded.request,
                &decision,
                pending_tool_call.as_ref(),
            )?
        };
        if let Some((turn, run_status)) = terminal {
            let turn = self.update_turn_from_run_status(turn, &run_status)?;
            self.append_native_trace(&turn.thread_id, &turn.turn_id, &run_status)?;
            messages.extend(self.agent_terminal_item_events(&run_status, &turn)?);
        }
        messages.push(
            JsonRpcMessage::response(message.id, json!({"decision": decision})).to_wire_value(),
        );
        Ok(messages)
    }

    fn native_approval_no_resume_status(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<&Value>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        let Ok(turn) = self.store.get_turn(&request.turn_id) else {
            return Ok(None);
        };
        if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            return Ok(None);
        }
        if pending_tool_call.is_none() {
            return Ok(None);
        }
        let (status, audit_decision, message) = match decision.outcome {
            ApprovalOutcome::Allow if pending_tool_call.is_some() => (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but native turn could not resume",
            ),
            ApprovalOutcome::Allow => (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but pending tool call is unavailable",
            ),
            ApprovalOutcome::Deny => (AgentStatus::Failed, "denied", "approval denied"),
            ApprovalOutcome::Defer => (AgentStatus::Blocked, "deferred", "approval deferred"),
        };
        let run_status = native_approval_terminal_status(
            &turn,
            request,
            decision,
            pending_tool_call,
            status,
            audit_decision,
            message,
        );
        Ok(Some((turn, run_status)))
    }

    fn event_subscribe(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EventSubscribeParams = message.params_as()?;
        self.event_filter = Some(params.event_types.clone());
        json_response(
            message.id,
            EventSubscribeResult {
                subscription_id: EVENT_SUBSCRIPTION_ID.to_string(),
                event_types: params.event_types,
            },
        )
    }

    fn artifact_fetch(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ArtifactFetchParams = message.params_as()?;
        match self.store.get_artifact_ref(&params.artifact_id) {
            Ok(artifact) => json_response(message.id, ArtifactFetchResult { artifact }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, ARTIFACT_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn event_notification(&self, event: AppEvent) -> Option<Value> {
        if self
            .event_filter
            .as_ref()
            .is_some_and(|event_types| !event_types.iter().any(|method| method == event.method()))
        {
            return None;
        }
        Some(event.to_notification().to_wire_value())
    }

    fn agent_terminal_item_events(
        &self,
        run_status: &AgentRunStatus,
        turn: &Turn,
    ) -> AppServerResult<Vec<Value>> {
        let Some(agent_delta) = agent_completed_delta(run_status) else {
            return Ok(Vec::new());
        };
        let agent_item = self.store.append_item(
            &turn.turn_id,
            singularity_protocol::ItemKind::AgentMessage,
            json!({"delta": agent_delta}),
        )?;
        Ok([
            AppEvent::item_started(agent_item.item_id.clone()),
            AppEvent::item_agent_message_delta(agent_item.item_id.clone(), agent_delta),
            AppEvent::item_completed(agent_item.item_id),
        ]
        .into_iter()
        .filter_map(|event| self.event_notification(event))
        .collect())
    }

    fn trace_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceListParams = message.params_as()?;
        match self
            .store
            .list_trace_page(&params.run_id, params.limit, params.offset)
        {
            Ok(events) => json_response(message.id, TraceListResult { events }),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_RUN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_show(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceShowParams = message.params_as()?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => Ok(vec![
                JsonRpcMessage::response(message.id, json!({"event": event})).to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_EVENT_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }

    fn trace_tail(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceTailParams = message.params_as()?;
        match self
            .store
            .tail_trace(&params.run_id, params.limit.unwrap_or(50), params.offset)
        {
            Ok(events) => Ok(vec![
                JsonRpcMessage::response(
                    message.id,
                    serde_json::to_value(TraceListResult { events })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => not_found_response(message.id, TRACE_RUN_NOT_FOUND),
            Err(error) => Err(error.into()),
        }
    }
}

fn json_response<T: serde::Serialize>(id: Option<Value>, result: T) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

fn native_approval_terminal_status(
    turn: &Turn,
    request: &ApprovalRequest,
    decision: &ApprovalDecision,
    pending_tool_call: Option<&Value>,
    status: AgentStatus,
    audit_decision: &str,
    message: impl Into<String>,
) -> AgentRunStatus {
    let mut run_status = AgentRunStatus::failed(message).with_status(status);
    run_status.run_id = Some(turn.turn_id.clone());
    run_status.session_id = Some(request.thread_id.clone());
    run_status.task_id = Some(turn.turn_id.clone());
    run_status.approval_count = 1;
    let mut audit_event = json!({
        "approval_policy": ApprovalPolicy::OnRequest,
        "approval_decision": audit_decision,
        "approval_request_id": decision.request_id,
        "approval_decision_id": decision.decision_id,
        "command_provenance": "agent_requested",
    });
    if let Some(command_audit) = pending_command_audit_metadata(pending_tool_call) {
        merge_json_object(&mut audit_event, command_audit);
    }
    run_status.audit_events.push(audit_event);
    run_status
}

fn pending_command_audit_metadata(pending_tool_call: Option<&Value>) -> Option<Value> {
    let pending_value = pending_tool_call?;
    let (tool_name, raw_arguments) =
        match serde_json::from_value::<PendingToolCall>(pending_value.clone()) {
            Ok(pending) => (pending.tool_name, Some(pending.raw_arguments)),
            Err(_error) => (
                pending_value
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                pending_value
                    .get("raw_arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
        };
    if tool_name != TOOL_COMMAND {
        return None;
    }
    let mut audit = json!({
        "sandbox_backend": "not_executed",
        "sandbox_enforcement": "strict",
    });
    let Some(raw_arguments) = raw_arguments else {
        audit["sandbox_mode"] = json!("unknown");
        audit["network_access"] = json!("unknown");
        audit["command_scope_digest"] = json!("unavailable");
        return Some(audit);
    };
    let Ok(input) = serde_json::from_str::<CommandToolInput>(&raw_arguments) else {
        audit["sandbox_mode"] = json!("unknown");
        audit["network_access"] = json!("unknown");
        audit["command_scope_digest"] = json!("unavailable");
        return Some(audit);
    };
    let sandbox_mode = input.sandbox_mode();
    let network_access = input.network_access();
    merge_json_object(
        &mut audit,
        json!({
            "sandbox_mode": sandbox_mode,
            "network_access": network_access,
            "command_scope_digest": command_scope_digest(
                &input.argv,
                &sandbox_mode,
                &network_access,
            ),
        }),
    );
    Some(audit)
}

fn merge_json_object(target: &mut Value, source: Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn native_workspace_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for spec in native_workspace_tool_specs() {
        registry
            .register(spec)
            .expect("valid native workspace tool");
    }
    registry
}

fn native_workspace_root(thread: &Thread) -> PathBuf {
    thread
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .map(Into::into)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| ".".into())
}

fn native_workspace_tools(workspace_root: PathBuf) -> WorkspaceTools {
    WorkspaceTools::new(workspace_root)
        .with_sandbox_backend(WindowsRestrictedTokenSandboxBackend::new())
}

fn native_loop_input(thread: &Thread, params: &TurnStartParams, turn_id: &str) -> AgentLoopInput {
    let goal = params
        .input
        .iter()
        .map(|item| match item {
            singularity_protocol::InputItem::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    AgentLoopInput::new(&params.thread_id, turn_id, goal).with_model_name(thread.model.clone())
}

struct EvalWorkspace {
    task_dir: PathBuf,
    repo_dir: PathBuf,
}

fn run_native_eval(params: &EvalRunParams, tasks: &[Value]) -> EvalRunResult {
    let run_dir =
        eval_output_root(params.output_root.as_deref()).join(safe_path_segment(&params.run_id));
    let task_results = tasks
        .iter()
        .map(|task| run_native_eval_task(task, &params.run_id, &run_dir))
        .collect::<Vec<_>>();
    let evaluation_passed = task_results
        .iter()
        .all(|task| task.get("evaluation_passed").and_then(Value::as_bool) == Some(true));
    let blocker = if evaluation_passed {
        None
    } else {
        task_results
            .iter()
            .filter_map(|task| task.get("blocker").and_then(Value::as_str))
            .next()
            .map(str::to_string)
            .or_else(|| Some(EVAL_VERIFICATION_BLOCKER.to_string()))
    };
    let status = if evaluation_passed {
        "completed"
    } else if task_results
        .iter()
        .any(|task| task.get("status").and_then(Value::as_str) == Some("blocked"))
    {
        "blocked"
    } else {
        "failed"
    };
    EvalRunResult {
        run_id: params.run_id.clone(),
        manifest: params.manifest.clone(),
        runner: EVAL_RUNNER_NAME.to_string(),
        status: status.to_string(),
        blocker,
        tasks: task_results,
        result_path: None,
        report_path: None,
        evaluation_passed,
    }
}

fn run_native_eval_blocked_by_capability(params: &EvalRunParams, tasks: &[Value]) -> EvalRunResult {
    let capability = AgentLoopCapability::current();
    let message = format!("{}: {}", capability.status.as_str(), capability.reason);
    let task_results = tasks
        .iter()
        .map(|task| blocked_eval_task_result(task, EVAL_CAPABILITY_BLOCKER, message.clone()))
        .collect::<Vec<_>>();
    EvalRunResult {
        run_id: params.run_id.clone(),
        manifest: params.manifest.clone(),
        runner: EVAL_RUNNER_NAME.to_string(),
        status: "blocked".to_string(),
        blocker: Some(EVAL_CAPABILITY_BLOCKER.to_string()),
        tasks: task_results,
        result_path: None,
        report_path: None,
        evaluation_passed: false,
    }
}

fn run_native_eval_task(task: &Value, run_id: &str, run_dir: &PathBuf) -> Value {
    let task_id = eval_task_id(task);
    if let Err(error) = validate_eval_workspace(task) {
        return blocked_eval_task_result(task, EVAL_WORKSPACE_BLOCKER, error);
    }
    let provider = match OpenAiProvider::from_env(|name| std::env::var(name).ok()) {
        Ok(provider) => provider,
        Err(error) => return blocked_eval_task_result(task, EVAL_PROVIDER_BLOCKER, error.message),
    };
    let workspace = match prepare_eval_workspace(task, run_dir) {
        Ok(workspace) => workspace,
        Err(error) => return blocked_eval_task_result(task, EVAL_WORKSPACE_BLOCKER, error),
    };
    if let Err(error) = run_prepare_commands(task, &workspace) {
        return blocked_eval_task_result(task, EVAL_WORKSPACE_BLOCKER, error);
    }
    let max_turns = task
        .pointer("/strategy/max_turns")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(EVAL_DEFAULT_MAX_TURNS);
    let agent_input = AgentLoopInput::new(
        &task_id,
        format!("{run_id}_{task_id}"),
        eval_agent_prompt(task),
    )
    .with_max_turns(max_turns);
    let agent_result = AgentLoop::new(
        provider,
        ToolBroker::new(native_workspace_registry()),
        native_eval_policy(workspace.repo_dir.to_string_lossy().into_owned(), task),
    )
    .with_workspace_tools(native_eval_workspace_tools(
        workspace.repo_dir.clone(),
        task,
    ))
    .run(&agent_input);
    let changed_files = git_changed_files(&workspace).unwrap_or_default();
    if let Err(error) = apply_eval_test_patch(task, &workspace) {
        return eval_task_result(
            task,
            &agent_result,
            changed_files,
            false,
            false,
            Some(EVAL_WORKSPACE_BLOCKER),
            Some(error),
            json!({"passed": false, "status": "not_run"}),
            json!({"passed": false, "status": "not_run"}),
            blocked_smoke_check_payload(task),
        );
    }
    let public_check = run_verification(task, &workspace, "public_verification_command")
        .or_else(|| run_verification(task, &workspace, "verification_command"))
        .unwrap_or_else(|| json!({"passed": false, "status": "not_run"}));
    let hidden_check = run_verification(task, &workspace, "hidden_verification_command")
        .unwrap_or_else(|| public_check.clone());
    let public_passed = public_check.get("passed").and_then(Value::as_bool) == Some(true);
    let hidden_passed = hidden_check.get("passed").and_then(Value::as_bool) == Some(true);
    let expected_change = expected_file_change_satisfied(task, &changed_files);
    let summary_ok = summary_requirement_satisfied(task, agent_result.final_answer.as_deref());
    let agent_completed = agent_result.completed;
    let smoke_check = run_smoke_check(task, &workspace, &agent_result);
    let smoke_command_satisfied = smoke_check.get("passed").and_then(Value::as_bool) == Some(true);
    let evaluation_passed = agent_completed
        && public_passed
        && hidden_passed
        && expected_change
        && summary_ok
        && smoke_command_satisfied;
    let blocker = if evaluation_passed {
        None
    } else if !agent_completed {
        Some(EVAL_AGENT_BLOCKER)
    } else {
        Some(EVAL_VERIFICATION_BLOCKER)
    };
    eval_task_result(
        task,
        &agent_result,
        changed_files,
        public_passed,
        hidden_passed,
        blocker,
        agent_result.error.clone(),
        public_check,
        hidden_check,
        smoke_check,
    )
}

fn prepare_eval_workspace(task: &Value, run_dir: &PathBuf) -> Result<EvalWorkspace, String> {
    let task_dir = run_dir.join(safe_path_segment(&eval_task_id(task)));
    if task_dir.exists() {
        std::fs::remove_dir_all(&task_dir).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&task_dir).map_err(|error| error.to_string())?;
    let repo_dir = task_dir.join(EVAL_REPO_DIR);
    let workspace = task
        .get("workspace")
        .and_then(Value::as_object)
        .ok_or_else(|| "eval task workspace must be an object".to_string())?;
    match workspace.get("type").and_then(Value::as_str) {
        Some("fixture") => {
            std::fs::create_dir_all(&repo_dir).map_err(|error| error.to_string())?;
            let files = workspace
                .get("files")
                .and_then(Value::as_object)
                .ok_or_else(|| "fixture workspace files must be an object".to_string())?;
            for (path, content) in files {
                let content = content
                    .as_str()
                    .ok_or_else(|| format!("fixture file content must be text: {path}"))?;
                write_eval_workspace_file(&repo_dir, path, content)?;
            }
        }
        Some("repo") => {
            let repo = workspace
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "repo workspace path must be a string".to_string())?;
            let clone = run_eval_command(
                &task_dir,
                &task_dir,
                vec![
                    "git".to_string(),
                    "clone".to_string(),
                    "--quiet".to_string(),
                    repo.to_string(),
                    EVAL_REPO_DIR.to_string(),
                ],
                EVAL_GIT_TIMEOUT_SECONDS,
                SandboxNetworkMode::Allowed,
                SandboxFilesystemMode::WorkspaceWrite,
            );
            ensure_command_success(&clone, "git clone")?;
            if let Some(commit) = workspace.get("start_commit").and_then(Value::as_str) {
                let checkout = run_eval_command(
                    &task_dir,
                    &repo_dir,
                    vec![
                        "git".to_string(),
                        "checkout".to_string(),
                        "--quiet".to_string(),
                        commit.to_string(),
                    ],
                    EVAL_GIT_TIMEOUT_SECONDS,
                    SandboxNetworkMode::Allowed,
                    SandboxFilesystemMode::WorkspaceWrite,
                );
                ensure_command_success(&checkout, "git checkout")?;
            }
        }
        Some(other) => return Err(format!("unsupported eval workspace type: {other}")),
        None => return Err("eval task workspace type is missing".to_string()),
    }
    Ok(EvalWorkspace { task_dir, repo_dir })
}

fn validate_eval_workspace(task: &Value) -> Result<(), String> {
    let workspace = task
        .get("workspace")
        .and_then(Value::as_object)
        .ok_or_else(|| "eval task workspace must be an object".to_string())?;
    match workspace.get("type").and_then(Value::as_str) {
        Some("fixture") => workspace
            .get("files")
            .and_then(Value::as_object)
            .map(|_| ())
            .ok_or_else(|| "fixture workspace files must be an object".to_string()),
        Some("repo") => workspace
            .get("path")
            .and_then(Value::as_str)
            .map(|_| ())
            .ok_or_else(|| "repo workspace path must be a string".to_string()),
        Some(other) => Err(format!("unsupported eval workspace type: {other}")),
        None => Err("eval task workspace type is missing".to_string()),
    }
}

fn run_prepare_commands(task: &Value, workspace: &EvalWorkspace) -> Result<(), String> {
    let Some(commands) = task.get("prepare_commands").and_then(Value::as_array) else {
        return Ok(());
    };
    for command in commands {
        let command = command
            .as_str()
            .ok_or_else(|| "prepare command must be a string".to_string())?;
        let result = run_eval_shell_command(
            workspace,
            command,
            EVAL_PREPARE_TIMEOUT_SECONDS,
            SandboxNetworkMode::Allowed,
            SandboxFilesystemMode::WorkspaceWrite,
        );
        ensure_command_success(&result, "prepare command")?;
    }
    Ok(())
}

fn run_verification(task: &Value, workspace: &EvalWorkspace, field: &str) -> Option<Value> {
    let command = task.get(field).and_then(Value::as_str)?;
    let timeout = task
        .get("verification_timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(EVAL_DEFAULT_TIMEOUT_SECONDS);
    let result = run_eval_shell_command(
        workspace,
        command,
        timeout,
        SandboxNetworkMode::Allowed,
        SandboxFilesystemMode::WorkspaceWrite,
    );
    Some(command_check_payload(result))
}

fn apply_eval_test_patch(task: &Value, workspace: &EvalWorkspace) -> Result<(), String> {
    let Some(patch) = task.get("test_patch").and_then(Value::as_str) else {
        return Ok(());
    };
    let patch_path = workspace.repo_dir.join(EVAL_TEST_PATCH_FILE);
    std::fs::write(&patch_path, patch).map_err(|error| error.to_string())?;
    let result = run_eval_command(
        &workspace.task_dir,
        &workspace.repo_dir,
        vec![
            "git".to_string(),
            "apply".to_string(),
            EVAL_TEST_PATCH_FILE.to_string(),
        ],
        EVAL_DEFAULT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Allowed,
        SandboxFilesystemMode::WorkspaceWrite,
    );
    let _ = std::fs::remove_file(&patch_path);
    ensure_command_success(&result, "git apply test patch")
}

fn git_changed_files(workspace: &EvalWorkspace) -> Result<Vec<String>, String> {
    let result = run_eval_command(
        &workspace.task_dir,
        &workspace.repo_dir,
        vec![
            "git".to_string(),
            "diff".to_string(),
            "--name-only".to_string(),
        ],
        EVAL_DEFAULT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Allowed,
        SandboxFilesystemMode::WorkspaceWrite,
    );
    ensure_command_success(&result, "git diff")?;
    Ok(result
        .stdout_preview
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn run_eval_shell_command(
    workspace: &EvalWorkspace,
    command: &str,
    timeout_seconds: u64,
    network_mode: SandboxNetworkMode,
    filesystem_mode: SandboxFilesystemMode,
) -> CommandResult {
    #[cfg(windows)]
    {
        let script_id = next_eval_command_id();
        let script_path = workspace.task_dir.join(format!("{script_id}.cmd"));
        if let Err(error) = std::fs::write(&script_path, format!("@echo off\r\n{command}\r\n")) {
            return CommandResult::spawn_failed(
                script_id,
                format!("failed to write eval command script: {error}"),
            );
        }
        let script_arg = eval_command_path_arg(&script_path);
        let result = run_eval_command(
            &workspace.task_dir,
            &workspace.repo_dir,
            vec!["cmd.exe".to_string(), "/C".to_string(), script_arg],
            timeout_seconds,
            network_mode,
            filesystem_mode,
        );
        let _ = std::fs::remove_file(script_path);
        result
    }
    #[cfg(not(windows))]
    {
        run_eval_command(
            &workspace.task_dir,
            &workspace.repo_dir,
            shell_argv(command),
            timeout_seconds,
            network_mode,
            filesystem_mode,
        )
    }
}

#[cfg(windows)]
fn eval_command_path_arg(path: &PathBuf) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    value.to_string()
}

fn run_eval_command(
    workspace_root: &PathBuf,
    cwd: &PathBuf,
    argv: Vec<String>,
    timeout_seconds: u64,
    network_mode: SandboxNetworkMode,
    filesystem_mode: SandboxFilesystemMode,
) -> CommandResult {
    let workspace_root = std::fs::canonicalize(workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let cwd = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut request =
        CommandRequest::project_verification(next_eval_command_id(), argv, cwd, workspace_root);
    request.timeout_seconds = timeout_seconds;
    request.network.mode = network_mode;
    request.filesystem.mode = filesystem_mode;
    WindowsRestrictedTokenSandboxBackend::new().execute(&request)
}

fn ensure_command_success(result: &CommandResult, label: &str) -> Result<(), String> {
    if result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
    {
        Ok(())
    } else {
        Err(format!("{label} failed: {}", result.stderr_preview))
    }
}

fn command_check_payload(result: CommandResult) -> Value {
    let passed = result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded;
    json!({
        "passed": passed,
        "status": if passed { "passed" } else { "failed" },
        "exit_code": result.exit_code,
        "execution_status": result.execution_status,
        "semantic_status": result.semantic_status,
        "stdout_preview": result.stdout_preview,
        "stderr_preview": result.stderr_preview,
        "timed_out": result.timed_out,
        "output_truncated": result.output_truncated,
    })
}

fn run_smoke_check(
    task: &Value,
    workspace: &EvalWorkspace,
    agent_result: &singularity_agent::AgentLoopResult,
) -> Value {
    if !smoke_command_required(task) {
        return json!({"passed": true, "status": "not_required"});
    }
    if smoke_command_satisfied(task, agent_result) {
        return json!({"passed": true, "status": "passed", "source": "agent_tool_result"});
    }
    let Some(command) = task.get("smoke_command").and_then(Value::as_str) else {
        return json!({"passed": false, "status": "not_run"});
    };
    let Some(argv) = parse_smoke_command_argv(command.trim()) else {
        return json!({"passed": false, "status": "not_run", "error": "invalid_smoke_command"});
    };
    let timeout = task
        .get("verification_timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(EVAL_DEFAULT_TIMEOUT_SECONDS);
    let mut payload = command_check_payload(run_eval_command(
        &workspace.task_dir,
        &workspace.repo_dir,
        argv,
        timeout,
        SandboxNetworkMode::Allowed,
        SandboxFilesystemMode::WorkspaceWrite,
    ));
    payload["source"] = json!("eval_runner_command");
    payload
}

fn eval_task_result(
    task: &Value,
    agent_result: &singularity_agent::AgentLoopResult,
    changed_files: Vec<String>,
    public_passed: bool,
    hidden_passed: bool,
    blocker: Option<&str>,
    error: Option<String>,
    public_check: Value,
    hidden_check: Value,
    smoke_check: Value,
) -> Value {
    let smoke_command_satisfied = smoke_check.get("passed").and_then(Value::as_bool) == Some(true);
    let evaluation_passed = blocker.is_none()
        && agent_result.completed
        && public_passed
        && hidden_passed
        && smoke_command_satisfied;
    json!({
        "task_id": eval_task_id(task),
        "agent_completed": agent_result.completed,
        "tests_passed": public_passed && hidden_passed,
        "public_verification_passed": public_passed,
        "hidden_verification_passed": hidden_passed,
        "smoke_command_satisfied": smoke_command_satisfied,
        "evaluation_passed": evaluation_passed,
        "local_process_fallback_count": 0,
        "status": if evaluation_passed { "completed" } else if blocker == Some(EVAL_WORKSPACE_BLOCKER) || blocker == Some(EVAL_PROVIDER_BLOCKER) { "blocked" } else { "failed" },
        "blocker": blocker,
        "error": error,
        "changed_files": changed_files,
        "model_turns": agent_result.model_turns,
        "tool_calls": agent_result.tool_calls,
        "approval_count": agent_result.approval_count,
        "checks": {
            "public": public_check,
            "hidden": hidden_check,
            "smoke": smoke_check,
        }
    })
}

fn blocked_eval_task_result(task: &Value, blocker: &str, message: String) -> Value {
    json!({
        "task_id": eval_task_id(task),
        "agent_completed": false,
        "tests_passed": false,
        "public_verification_passed": false,
        "hidden_verification_passed": false,
        "smoke_command_satisfied": !smoke_command_required(task),
        "evaluation_passed": false,
        "local_process_fallback_count": 0,
        "status": "blocked",
        "blocker": blocker,
        "error": message,
        "checks": {
            "public": {"passed": false, "status": "not_run"},
            "hidden": {"passed": false, "status": "not_run"},
            "smoke": blocked_smoke_check_payload(task)
        }
    })
}

fn smoke_command_required(task: &Value) -> bool {
    task.get("smoke_command")
        .and_then(Value::as_str)
        .is_some_and(|command| !command.trim().is_empty())
}

fn smoke_command_satisfied(
    task: &Value,
    agent_result: &singularity_agent::AgentLoopResult,
) -> bool {
    let Some(expected_result_id) = expected_smoke_command_result_id(task) else {
        return !smoke_command_required(task);
    };
    agent_result.tool_results.iter().any(|result| {
        result.tool_name == TOOL_COMMAND
            && result.ok
            && result.result_id.as_deref() == Some(expected_result_id.as_str())
    })
}

fn expected_smoke_command_result_id(task: &Value) -> Option<String> {
    let command = task.get("smoke_command").and_then(Value::as_str)?.trim();
    if command.is_empty() {
        return None;
    }
    let argv = parse_smoke_command_argv(command)?;
    Some(command_scope_digest(
        &argv,
        &SandboxFilesystemMode::WorkspaceWrite,
        &SandboxNetworkMode::Allowed,
    ))
}

fn expected_smoke_command_resource(task: &Value) -> Option<String> {
    let command = task.get("smoke_command").and_then(Value::as_str)?.trim();
    if command.is_empty() {
        return None;
    }
    let argv = parse_smoke_command_argv(command)?;
    Some(command_scope_resource(
        &argv,
        &SandboxFilesystemMode::WorkspaceWrite,
        &SandboxNetworkMode::Allowed,
    ))
}

fn parse_smoke_command_argv(command: &str) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match quote {
            Some(marker) if ch == marker => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        argv.push(current);
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(test)]
fn smoke_check_payload(task: &Value, satisfied: bool) -> Value {
    if !smoke_command_required(task) {
        return json!({"passed": true, "status": "not_required"});
    }
    let status = if satisfied {
        "passed"
    } else if expected_smoke_command_result_id(task).is_some() {
        "failed"
    } else {
        "not_run"
    };
    json!({
        "passed": satisfied,
        "status": status
    })
}

fn blocked_smoke_check_payload(task: &Value) -> Value {
    if smoke_command_required(task) {
        json!({"passed": false, "status": "not_run"})
    } else {
        json!({"passed": true, "status": "not_required"})
    }
}

fn native_eval_workspace_tools(workspace_root: PathBuf, _task: &Value) -> WorkspaceTools {
    WorkspaceTools::new(workspace_root)
        .with_sandbox_backend(WindowsRestrictedTokenSandboxBackend::new())
}

fn native_eval_policy(workspace_root: String, task: &Value) -> PolicyEngine {
    let mut profile = PermissionProfile::workspace_write(workspace_root);
    profile.approval_policy = ApprovalPolicy::Never;
    let mut policy = PolicyEngine::new(profile).with_rule(native_read_tool_rule());
    if let Some(resource) = expected_smoke_command_resource(task) {
        policy = policy.with_rule(native_execute_tool_rule(resource));
    }
    if let Some(paths) = task.get("allowed_paths").and_then(Value::as_array) {
        for (index, path) in paths.iter().filter_map(Value::as_str).enumerate() {
            policy = policy.with_rule(native_write_tool_rule(index, path));
        }
    } else {
        policy = policy.with_rule(native_write_tool_rule(0, ""));
    }
    policy
}

fn native_write_tool_rule(index: usize, path: &str) -> PermissionRule {
    let rule = PermissionRule::new(
        format!("allow_native_eval_write_tool_{index}"),
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Write);
    if path.is_empty() {
        rule
    } else {
        rule.for_resource(path)
    }
}

fn native_execute_tool_rule(resource: String) -> PermissionRule {
    PermissionRule::new(
        "allow_native_eval_command_tools",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Execute)
    .for_resource(resource)
}

fn eval_agent_prompt(task: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(user_task) = task.get("user_task").and_then(Value::as_str) {
        parts.push(user_task.to_string());
    }
    if let Some(paths) = task.get("allowed_paths").and_then(Value::as_array) {
        let allowed = paths
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !allowed.is_empty() {
            parts.push(format!("Only modify these workspace paths unless the task proves another path is required: {allowed}."));
        }
    }
    if let Some(command) = task.get("smoke_command").and_then(Value::as_str) {
        let instruction = if let Some(argv) = parse_smoke_command_argv(command.trim()) {
            let payload = json!({
                "argv": argv,
                "sandbox_mode": "workspace_write",
                "network_access": "allowed",
            });
            format!(
                "Before the final answer, call the command tool exactly once with these arguments: {payload}. The evaluation fails if this command tool result is missing."
            )
        } else {
            format!(
                "Before the final answer, use the command tool with argv to run this smoke command: {command}. The evaluation fails if this command tool result is missing."
            )
        };
        parts.push(instruction);
    }
    parts.push("Available tools are read, list, grep, edit, patch, and command. Finish with a concise final answer that mentions verification.".to_string());
    parts.join("\n\n")
}

fn expected_file_change_satisfied(task: &Value, changed_files: &[String]) -> bool {
    let Some(expected) = task.get("expected_file_changes").and_then(Value::as_array) else {
        return true;
    };
    expected.iter().filter_map(Value::as_str).all(|path| {
        changed_files
            .iter()
            .any(|changed| changed.replace('\\', "/") == path.replace('\\', "/"))
    })
}

fn summary_requirement_satisfied(task: &Value, final_answer: Option<&str>) -> bool {
    let Some(required) = task
        .pointer("/success/summary_contains")
        .and_then(Value::as_str)
    else {
        return true;
    };
    final_answer
        .map(|answer| {
            answer
                .to_ascii_lowercase()
                .contains(&required.to_ascii_lowercase())
        })
        .unwrap_or(false)
}

fn write_eval_workspace_file(root: &PathBuf, relative: &str, content: &str) -> Result<(), String> {
    if relative.contains("..") || relative.starts_with('/') || relative.starts_with('\\') {
        return Err(format!(
            "fixture file path is outside workspace: {relative}"
        ));
    }
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, content).map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn shell_argv(command: &str) -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string(), command.to_string()]
}

fn eval_task_id(task: &Value) -> String {
    task.get("task_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn next_eval_command_id() -> String {
    let sequence = EVAL_COMMAND_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("eval_command_{sequence}")
}

fn write_eval_artifacts(
    result: &mut EvalRunResult,
    output_root: Option<&str>,
) -> Result<(), String> {
    let run_dir = eval_output_root(output_root).join(safe_path_segment(&result.run_id));
    std::fs::create_dir_all(&run_dir).map_err(|error| error.to_string())?;
    let result_path = run_dir.join("result.json");
    let report_path = run_dir.join("report.json");
    result.result_path = Some(result_path.to_string_lossy().to_string());
    result.report_path = Some(report_path.to_string_lossy().to_string());
    let payload = eval_result_payload(result, &run_dir);
    let serialized = serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?;
    std::fs::write(&result_path, format!("{serialized}\n")).map_err(|error| error.to_string())?;
    std::fs::write(&report_path, format!("{serialized}\n")).map_err(|error| error.to_string())?;
    Ok(())
}

fn eval_result_payload(result: &EvalRunResult, run_dir: &std::path::Path) -> Value {
    let total = result.tasks.len();
    let passed = result
        .tasks
        .iter()
        .filter(|task| task.get("evaluation_passed").and_then(Value::as_bool) == Some(true))
        .count();
    json!({
        "schema_version": EVAL_RESULT_SCHEMA,
        "run_id": &result.run_id,
        "output_dir": run_dir.to_string_lossy(),
        "runner": &result.runner,
        "status": &result.status,
        "blocker": &result.blocker,
        "summary": {
            "total": total,
            "passed": passed,
            "failed": total.saturating_sub(passed),
            "evaluation_passed": result.evaluation_passed,
        },
        "tasks": &result.tasks,
        "result_path": &result.result_path,
        "report_path": &result.report_path,
        "evaluation_passed": result.evaluation_passed,
    })
}

fn eval_output_root(output_root: Option<&str>) -> PathBuf {
    output_root
        .map(PathBuf::from)
        .or_else(|| std::env::var(EVAL_OUTPUT_DIR_ENV).ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("work").join("evaluations"))
}

fn safe_path_segment(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.trim_matches('.').is_empty() {
        "eval_run".to_string()
    } else {
        safe
    }
}

fn native_agent_loop_ready() -> bool {
    let capability = AgentLoopCapability::current();
    native_capability_ready(&capability)
}

fn native_capability_ready(capability: &AgentLoopCapability) -> bool {
    capability.available
        && capability.blockers.is_empty()
        && capability.status == AgentStatus::Completed
}

fn native_agent_loop_unavailable_message(capability: &AgentLoopCapability) -> String {
    let blockers = if capability.blockers.is_empty() {
        "none".to_string()
    } else {
        capability.blockers.join(",")
    };
    format!(
        "Rust AgentLoop is not available: status={}; blockers={blockers}",
        capability.status.as_str()
    )
}
fn native_workspace_policy(workspace_root: String) -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write(workspace_root))
        .with_rule(native_read_tool_rule())
}

fn native_read_tool_rule() -> PermissionRule {
    PermissionRule::new(
        "allow_native_read_tools",
        SettingsScope::Project,
        PermissionDecisionOutcome::Allow,
    )
    .for_operation(PermissionOperation::Read)
}

fn native_workspace_tool_specs() -> Vec<ToolSpec> {
    let command_sandbox_modes = ["read_only", "workspace_write", "danger_full_access"];
    let command_network_access = ["denied", "allowed"];
    vec![
        ToolSpec::new(
            TOOL_READ,
            "Read a workspace file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_chars": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_LIST,
            "List workspace directory entries",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_entries": {"type": "integer", "minimum": 1}
                },
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_GREP,
            "Search workspace text files",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "pattern": {"type": "string"},
                    "max_matches": {"type": "integer", "minimum": 1}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_EDIT,
            "Replace expected text in a workspace file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "expected": {"type": "string"},
                    "replacement": {"type": "string"}
                },
                "required": ["path", "expected", "replacement"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_PATCH,
            "Apply explicit workspace file changes",
            json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "expected": {"type": ["string", "null"]},
                                "replacement": {"type": "string"}
                            },
                            "required": ["path", "replacement"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["changes"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            TOOL_COMMAND,
            "Run a sandboxed command",
            json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1
                    },
                    "cwd": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1},
                    "sandbox_mode": {"type": "string", "enum": command_sandbox_modes},
                    "network_access": {"type": "string", "enum": command_network_access, "default": "allowed"}
                },
                "required": ["argv"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn json_error(id: Option<Value>, error: ErrorCode) -> AppServerResult<Vec<Value>> {
    Ok(vec![JsonRpcMessage::error(id, error).to_wire_value()])
}

fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    !matches!(status, TurnStatus::Running)
}

fn turn_status_str(status: &TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Blocked => "blocked",
        TurnStatus::Failed => "failed",
        TurnStatus::Interrupted => "interrupted",
    }
}

fn agent_completed_delta(run_status: &AgentRunStatus) -> Option<String> {
    if run_status.status == AgentStatus::Completed {
        run_status
            .final_answer
            .as_deref()
            .filter(|answer| !answer.trim().is_empty())
            .map(redact_app_server_text)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use singularity_agent::{AgentLoopResult, PendingToolCall};
    use singularity_model::{
        ModelToolCall, ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, Provider,
        ProviderError,
    };
    use singularity_policy::PermissionRequest;
    use singularity_tools::{ToolResult, command_scope_digest, command_scope_resource};

    use super::*;

    struct StaticProvider {
        responses: Vec<ModelTurnResponse>,
        seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    }

    impl Provider for StaticProvider {
        fn complete(&self, request: &ModelTurnRequest) -> Result<ModelTurnResponse, ProviderError> {
            let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
            let response_index = seen_requests.len();
            seen_requests.push(request.clone());
            Ok(self
                .responses
                .get(response_index)
                .unwrap_or_else(|| self.responses.last().expect("static provider response"))
                .clone())
        }
    }

    #[test]
    fn native_eval_smoke_gate_requires_matching_command_scope_digest() {
        let task = json!({"task_id": "smoke", "smoke_command": "python -m py_compile src/app.py"});
        let expected_id = expected_smoke_command_result_id(&task).expect("expected smoke digest");
        let wrong_id = command_scope_digest(
            &[
                "python".to_string(),
                "-c".to_string(),
                "print('ok')".to_string(),
            ],
            &SandboxFilesystemMode::ReadOnly,
            &SandboxNetworkMode::Allowed,
        );
        let mut wrong_tool = ToolResult::summary(
            "call_wrong",
            TOOL_COMMAND,
            true,
            "wrong command ok",
            "digest_wrong",
        );
        wrong_tool.result_id = Some(wrong_id);
        let mut right_tool = ToolResult::summary(
            "call_right",
            TOOL_COMMAND,
            true,
            "smoke command ok",
            "digest_right",
        );
        right_tool.result_id = Some(expected_id);
        let base_result = AgentLoopResult {
            status: AgentStatus::Completed,
            completed: true,
            final_answer: Some("done".to_string()),
            model_turns: 1,
            tool_calls: 1,
            approval_count: 0,
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_results: vec![wrong_tool],
            tool_repairs: Vec::new(),
            error: None,
        };

        assert!(!smoke_command_satisfied(&task, &base_result));
        assert_eq!(smoke_check_payload(&task, false)["status"], "failed");

        let mut matching_result = base_result;
        matching_result.tool_results = vec![right_tool];
        assert!(smoke_command_satisfied(&task, &matching_result));
        assert_eq!(smoke_check_payload(&task, true)["status"], "passed");
    }

    #[test]
    fn native_eval_prompt_includes_exact_smoke_command_tool_arguments() {
        let task = json!({
            "user_task": "fix it",
            "smoke_command": "python -m py_compile src/app.py"
        });

        let prompt = eval_agent_prompt(&task);

        assert!(prompt.contains("\"argv\":[\"python\",\"-m\",\"py_compile\",\"src/app.py\"]"));
        assert!(prompt.contains("\"sandbox_mode\":\"workspace_write\""));
        assert!(prompt.contains("\"network_access\":\"allowed\""));
        assert!(prompt.contains("evaluation fails if this command tool result is missing"));
    }

    #[test]
    fn native_eval_policy_only_allows_exact_smoke_command_resource() {
        let task = json!({"task_id": "smoke", "smoke_command": "python -m py_compile src/app.py"});
        let policy = native_eval_policy("C:/repo".to_string(), &task);
        let expected_resource = expected_smoke_command_resource(&task).expect("smoke resource");
        let wrong_resource = command_scope_resource(
            &[
                "python".to_string(),
                "-c".to_string(),
                "print('ok')".to_string(),
            ],
            &SandboxFilesystemMode::WorkspaceWrite,
            &SandboxNetworkMode::Allowed,
        );
        let wider_resource = command_scope_resource(
            &[
                "python".to_string(),
                "-m".to_string(),
                "py_compile".to_string(),
                "src/app.py".to_string(),
            ],
            &SandboxFilesystemMode::DangerFullAccess,
            &SandboxNetworkMode::Allowed,
        );

        let allowed = policy.evaluate(&PermissionRequest::new(
            TOOL_COMMAND,
            PermissionOperation::Execute,
            expected_resource,
        ));
        let wrong = policy.evaluate(&PermissionRequest::new(
            TOOL_COMMAND,
            PermissionOperation::Execute,
            wrong_resource,
        ));
        let wider = policy.evaluate(&PermissionRequest::new(
            TOOL_COMMAND,
            PermissionOperation::Execute,
            wider_resource,
        ));

        assert_eq!(allowed.outcome, PermissionDecisionOutcome::Allow);
        assert_eq!(wrong.outcome, PermissionDecisionOutcome::Deny);
        assert_eq!(wider.outcome, PermissionDecisionOutcome::Deny);
    }

    #[cfg(windows)]
    #[test]
    fn native_eval_smoke_check_runs_real_command_when_agent_omits_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(repo_dir.join("src")).expect("src dir");
        std::fs::write(repo_dir.join("src").join("app.py"), "print('ok')\n").expect("app file");
        let workspace = EvalWorkspace {
            task_dir: dir.path().to_path_buf(),
            repo_dir,
        };
        let task = json!({"task_id": "smoke", "smoke_command": "python -m py_compile src/app.py"});
        let agent_result = AgentLoopResult {
            status: AgentStatus::Completed,
            completed: true,
            final_answer: Some("done".to_string()),
            model_turns: 1,
            tool_calls: 0,
            approval_count: 0,
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_results: Vec::new(),
            tool_repairs: Vec::new(),
            error: None,
        };

        let payload = run_smoke_check(&task, &workspace, &agent_result);

        assert_eq!(payload["passed"], true);
        assert_eq!(payload["status"], "passed");
        assert_eq!(payload["source"], "eval_runner_command");
    }

    #[test]
    fn native_approval_resume_without_pending_tool_call_fails_closed_after_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let server = AppServer::new(store);
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            turn.turn_id.clone(),
            turn.turn_id.clone(),
            TOOL_EDIT,
        )
        .with_thread_turn_binding(thread.thread_id.clone(), turn.turn_id.clone())
        .with_tool_call_id("call_1")
        .with_resources(["README.md"]);
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![final_response],
            seen_requests: Arc::clone(&seen_requests),
        };

        let resumed = server
            .resume_native_agent_loop_after_gate(&request, &decision, None, provider)
            .expect("resume");

        assert!(resumed.is_none());
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "before"
        );
        assert!(seen_requests.lock().expect("seen requests").is_empty());
    }

    #[test]
    fn native_approval_no_resume_status_records_session_and_command_audit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "run command"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            turn.turn_id.clone(),
            turn.turn_id.clone(),
            TOOL_COMMAND,
        )
        .with_thread_turn_binding(thread.thread_id.clone(), turn.turn_id.clone())
        .with_tool_call_id("call_1");
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: json!({
                "argv": ["python", "-c", "print('ok')"],
                "sandbox_mode": "workspace_write",
                "network_access": "allowed"
            })
            .to_string(),
            resources: Vec::new(),
        };
        let pending_payload = serde_json::to_value(&pending).expect("pending payload");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let server = AppServer::new(store);

        let (_turn, run_status) = server
            .native_approval_no_resume_status(&request, &decision, Some(&pending_payload))
            .expect("status")
            .expect("terminal status");

        assert_eq!(
            run_status.session_id.as_deref(),
            Some(thread.thread_id.as_str())
        );
        assert_eq!(
            run_status.audit_events[0]["sandbox_mode"],
            "workspace_write"
        );
        assert_eq!(run_status.audit_events[0]["network_access"], "allowed");
        assert_eq!(
            run_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert_eq!(run_status.audit_events[0]["sandbox_enforcement"], "strict");
        assert!(
            run_status.audit_events[0]["command_scope_digest"]
                .as_str()
                .expect("command scope digest")
                .starts_with("hash:")
        );
        assert_eq!(
            run_status.audit_events[0]["approval_decision"],
            "unavailable"
        );
    }

    #[test]
    fn native_approval_resume_failures_record_command_audit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "run command"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            turn.turn_id.clone(),
            turn.turn_id.clone(),
            TOOL_COMMAND,
        )
        .with_thread_turn_binding(thread.thread_id.clone(), turn.turn_id.clone())
        .with_tool_call_id("call_1");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let valid_command = json!({
            "argv": ["python", "-c", "print('ok')"],
            "sandbox_mode": "workspace_write",
            "network_access": "allowed"
        })
        .to_string();
        let mismatched_pending = PendingToolCall {
            request_id: "approval_other_call_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: valid_command.clone(),
            resources: Vec::new(),
        };
        let invalid_args_pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_COMMAND.to_string(),
            raw_arguments: "{not-json".to_string(),
            resources: Vec::new(),
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let server = AppServer::new(store);

        let (_turn, mismatch_status) = server
            .resume_native_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&mismatched_pending).expect("pending payload")),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "done",
                    )],
                    seen_requests: Arc::clone(&seen_requests),
                },
            )
            .expect("resume")
            .expect("terminal status");
        assert_eq!(mismatch_status.status, AgentStatus::Failed);
        assert_eq!(
            mismatch_status.audit_events[0]["sandbox_backend"],
            "not_executed"
        );
        assert!(
            mismatch_status.audit_events[0]["command_scope_digest"]
                .as_str()
                .expect("command scope digest")
                .starts_with("hash:")
        );

        let (_turn, invalid_args_status) = server
            .resume_native_agent_loop_after_gate(
                &request,
                &decision,
                Some(serde_json::to_value(&invalid_args_pending).expect("pending payload")),
                StaticProvider {
                    responses: vec![ModelTurnResponse::completed(
                        "model_request_turn_1_0",
                        "response_1",
                        "done",
                    )],
                    seen_requests: Arc::clone(&seen_requests),
                },
            )
            .expect("resume")
            .expect("terminal status");
        assert_eq!(invalid_args_status.status, AgentStatus::Failed);
        assert_eq!(
            invalid_args_status.audit_events[0]["approval_decision"],
            "unavailable"
        );
        assert_eq!(
            invalid_args_status.audit_events[0]["sandbox_enforcement"],
            "strict"
        );
        assert_eq!(
            invalid_args_status.audit_events[0]["command_scope_digest"],
            "unavailable"
        );
        assert!(seen_requests.lock().expect("seen requests").is_empty());
    }

    #[test]
    fn native_approval_resume_uses_stored_pending_tool_call_after_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let file_path = workspace.join("README.md");
        std::fs::write(&file_path, "before").expect("write readme");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _item, _trace) = store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Blocked.as_str(),
                json!([{"type": "text", "text": "edit readme"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                TurnStatus::Blocked,
                AgentStatus::Blocked.as_str(),
            )
            .expect("blocked turn");
        let request = ApprovalRequest::new(
            format!("approval_{}_call_1", turn.turn_id),
            turn.turn_id.clone(),
            turn.turn_id.clone(),
            TOOL_EDIT,
        )
        .with_thread_turn_binding(thread.thread_id.clone(), turn.turn_id.clone())
        .with_tool_call_id("call_1")
        .with_resources(["README.md"]);
        let pending = PendingToolCall {
            request_id: request.request_id.clone(),
            tool_call_id: "call_1".to_string(),
            tool_name: TOOL_EDIT.to_string(),
            raw_arguments: json!({
                "path": "README.md",
                "expected": "before",
                "replacement": "after"
            })
            .to_string(),
            resources: vec!["README.md".to_string()],
        };
        let pending_payload = serde_json::to_value(&pending).expect("pending payload");
        let decision = ApprovalDecision::new(
            request.request_id.clone(),
            ApprovalOutcome::Allow,
            "approved",
        );
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![final_response],
            seen_requests: Arc::clone(&seen_requests),
        };
        let server = AppServer::new(store);

        let resumed = server
            .resume_native_agent_loop_after_gate(
                &request,
                &decision,
                Some(pending_payload),
                provider,
            )
            .expect("resume")
            .expect("resumed");

        assert_eq!(resumed.0.turn_id, turn.turn_id);
        assert_eq!(resumed.1.status, AgentStatus::Completed);
        assert_eq!(resumed.1.final_answer.as_deref(), Some("done"));
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read readme"),
            "after"
        );
        assert_eq!(seen_requests.lock().expect("seen requests").len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn native_agent_loop_command_uses_restricted_token_backend_after_gate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let params = TurnStartParams {
            thread_id: thread.thread_id.clone(),
            input: vec![singularity_protocol::InputItem::Text {
                text: "run command".to_string(),
            }],
        };
        let mut command_response =
            ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
        command_response.tool_calls.push(ModelToolCall {
            tool_call_id: "call_1".to_string(),
            tool_name: "builtin.command".to_string(),
            arguments: json!({
                "argv": ["cmd.exe", "/C", "echo app-server-sandbox-ok"],
                "timeout_seconds": 5
            }),
            raw_arguments: json!({
                "argv": ["cmd.exe", "/C", "echo app-server-sandbox-ok"],
                "timeout_seconds": 5
            })
            .to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
            provider_metadata: json!({}),
        });
        let final_response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
        let provider = StaticProvider {
            responses: vec![command_response, final_response],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        };
        let server = AppServer::new(store);
        let command_resource = command_scope_resource(
            &[
                "cmd.exe".to_string(),
                "/C".to_string(),
                "echo app-server-sandbox-ok".to_string(),
            ],
            &SandboxFilesystemMode::ReadOnly,
            &SandboxNetworkMode::Allowed,
        );
        let grant = ApprovalGrant::allow(
            "approval_turn_1_call_1",
            "builtin.command",
            [command_resource],
        );

        let status = server
            .run_native_agent_loop_with_provider(provider, &thread, &params, "turn_1", Some(grant))
            .expect("native loop");

        assert_eq!(status.status, AgentStatus::Completed);
        assert_eq!(status.final_answer.as_deref(), Some("done"));
        assert_eq!(status.tool_calls, 1);
    }
}

fn redact_app_server_text(text: &str) -> String {
    if contains_sensitive_text(text) {
        "[redacted sensitive app-server output]".to_string()
    } else {
        text.to_string()
    }
}

fn not_found_response(id: Option<Value>, message: &'static str) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::not_found(message)).to_wire_value(),
    ])
}

fn invalid_request_response(
    id: Option<Value>,
    message: impl Into<String>,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::invalid_request(message)).to_wire_value(),
    ])
}
