use singularity_agent::{
    AgentContextItem, AgentContextItemPriority, AgentLoop, AgentLoopCapability, AgentLoopInput,
    AgentLoopStep, AgentStatus, ApprovalGrant, assemble_context_items,
};
use singularity_core::CancellationToken;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, Provider, ProviderError,
};
use singularity_policy::{
    NetworkAccess, PermissionDecisionOutcome, PermissionOperation, PermissionProfile,
    PermissionProfileName, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_tools::{
    CommandRequest, CommandResult, SandboxBackend, SandboxCapabilities, SandboxFilesystemMode,
    SandboxNetworkMode, ToolBroker, ToolRegistry, ToolSpec, WorkspaceTools, command_scope_digest,
    command_scope_resource,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl Provider for StaticProvider {
    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
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

struct BlockingProvider {
    started: Mutex<Option<mpsc::Sender<()>>>,
}

impl Provider for BlockingProvider {
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        if let Some(started) = self.started.lock().expect("started lock").take() {
            started.send(()).expect("signal provider start");
        }
        while !cancellation.is_cancelled() {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "response_after_cancel",
            "must not complete",
        ))
    }
}

fn agent_loop_with_response(
    response: ModelTurnResponse,
    policy: PolicyEngine,
) -> AgentLoop<StaticProvider> {
    agent_loop_with_response_and_requests(response, policy, Arc::new(Mutex::new(Vec::new())))
}

fn agent_loop_with_response_and_requests(
    response: ModelTurnResponse,
    policy: PolicyEngine,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
) -> AgentLoop<StaticProvider> {
    agent_loop_with_responses_and_requests(vec![response], policy, seen_requests)
}

fn agent_loop_with_responses_and_requests(
    responses: Vec<ModelTurnResponse>,
    policy: PolicyEngine,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
) -> AgentLoop<StaticProvider> {
    let mut registry = ToolRegistry::default();
    registry
        .register(ToolSpec::new(
            "builtin.read",
            "Read file",
            serde_json::json!({"type": "object"}),
        ))
        .expect("register builtin read");
    registry
        .register(ToolSpec::new(
            "builtin.edit",
            "Edit file",
            serde_json::json!({"type": "object"}),
        ))
        .expect("register builtin edit");
    registry
        .register(ToolSpec::new(
            "builtin.patch",
            "Apply patch",
            serde_json::json!({"type": "object"}),
        ))
        .expect("register builtin patch");
    registry
        .register(ToolSpec::new(
            "builtin.command",
            "Run command",
            serde_json::json!({"type": "object"}),
        ))
        .expect("register builtin command");
    AgentLoop::new(
        StaticProvider {
            responses,
            seen_requests,
        },
        ToolBroker::new(registry),
        policy,
    )
}

fn allow_read_policy() -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_read",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Read),
    )
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        raw_arguments: arguments.to_string(),
        arguments,
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}

fn test_command(argument: &str) -> Vec<String> {
    vec!["test-program".to_string(), argument.to_string()]
}

#[cfg(windows)]
#[test]
fn agent_loop_capability_is_available_without_remaining_blockers() {
    let capability = AgentLoopCapability::current();

    assert!(capability.available);
    assert_eq!(capability.status, AgentStatus::Completed);
    assert!(capability.reason.contains("native Rust AgentLoop"));
    assert!(capability.blockers.is_empty());
}

#[cfg(not(windows))]
#[test]
fn agent_loop_capability_reports_unsupported_platform_blocker() {
    let capability = AgentLoopCapability::current();

    assert!(!capability.available);
    assert_eq!(capability.status, AgentStatus::Blocked);
    assert!(
        capability
            .blockers
            .contains(&"strict_command_sandbox_unsupported_platform".to_string())
    );
}

#[test]
fn agent_loop_plan_lists_real_integration_steps() {
    let plan = AgentLoop::<StaticProvider>::integration_plan();

    assert_eq!(
        plan.steps,
        vec![
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
        ]
    );
    assert!(plan.blockers.is_empty());
}

#[test]
fn agent_loop_read_only_final_answer_completes_without_verification() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello");
    let result = agent_loop_with_response(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
    )
    .run(&input);
    let run_status = result.to_run_status(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert!(result.completed);
    assert_eq!(result.model_turns, 1);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert!(result.error.is_none());
    assert_eq!(run_status.status, AgentStatus::Completed);
    assert_eq!(run_status.final_answer.as_deref(), Some("done"));
    assert_eq!(run_status.run_id.as_deref(), Some("turn_1"));
    assert_eq!(run_status.model_turns, 1);
    assert_eq!(run_status.tool_calls, 0);
    assert!(!run_status.verification.required);
    assert!(!run_status.verification.passed);
}

#[test]
fn agent_loop_cancels_while_waiting_for_provider_and_discards_late_completion() {
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        AgentLoop::new(
            BlockingProvider {
                started: Mutex::new(Some(started_tx)),
            },
            ToolBroker::new(ToolRegistry::default()),
            allow_read_policy(),
        )
        .with_cancellation_token(worker_cancellation)
        .run(&AgentLoopInput::new("thread_1", "turn_1", "wait"))
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("provider started");
    cancellation.cancel();
    let result = worker.join().expect("agent worker joins");

    assert_eq!(result.status, AgentStatus::Cancelled);
    assert!(!result.completed);
    assert!(result.final_answer.is_none());
    assert_eq!(result.model_turns, 1);
}

#[test]
fn agent_loop_rejects_final_after_mutation_without_verification() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "change the file")
    };
    let mut edit = ModelTurnResponse::completed("request_1", "response_1", "");
    edit.tool_calls.push(tool_call(
        "call_1",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let final_response = ModelTurnResponse::completed("request_2", "response_2", "done");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "allow_write",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write),
        )
        .with_rule(
            PermissionRule::new(
                "allow_execute",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute),
        );

    let result = agent_loop_with_responses_and_requests(
        vec![edit, final_response],
        policy,
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "completion gate rejected final answer: verification required after workspace mutation"
        )
    );
    assert!(result.verification.required);
    assert!(!result.verification.passed);
    assert_eq!(result.verification.successful_command_count, 0);
}

#[test]
fn agent_loop_unknown_tool_does_not_execute_and_fails_closed_after_budget() {
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.missing",
        serde_json::json!({}),
    ));

    let result = agent_loop_with_response(response, allow_read_policy()).run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("unknown_tool")
    );
    assert_eq!(
        result.error.as_deref(),
        Some("tool execution failed: unknown_tool")
    );
}

#[test]
fn agent_loop_ask_decision_blocks_without_executing_tool() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.read",
        serde_json::json!({"path": "README.md"}),
    ));

    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
}

#[test]
fn agent_loop_sends_project_instructions_as_developer_message_without_serializing_them() {
    let project_instructions = "root instructions\n\nchild instructions";
    let input = AgentLoopInput::new("thread_1", "turn_1", "user goal")
        .with_project_instructions(project_instructions);
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests lock");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    let developer = requests[0].messages[0].content[0]
        .text
        .as_deref()
        .expect("developer instructions");
    assert!(developer.contains("You are a coding agent working in the current workspace."));
    assert!(developer.ends_with(project_instructions));
    assert_eq!(
        requests[0].messages[1].content[0].text.as_deref(),
        Some("user goal")
    );
    assert!(
        !requests[0].messages[1].content[0]
            .text
            .as_deref()
            .unwrap_or_default()
            .contains(project_instructions)
    );
    assert!(!requests[0].tools.iter().any(|tool| {
        serde_json::to_string(tool)
            .expect("serialize tool")
            .contains(project_instructions)
    }));
    assert!(
        !requests[0]
            .trace_metadata
            .to_string()
            .contains(project_instructions)
    );
    assert!(
        !serde_json::to_string(&input)
            .expect("serialize input")
            .contains(project_instructions)
    );
}

#[test]
fn agent_loop_model_request_orders_developer_history_and_current_turn() {
    let project_instructions = "root instructions";
    let input = AgentLoopInput::new("thread_1", "turn_1", "current user")
        .with_project_instructions(project_instructions)
        .with_history([
            AgentContextItem::history_user("history_user_1", "previous user"),
            AgentContextItem::history_assistant("history_assistant_1", "previous assistant"),
        ]);
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests lock");
    let request_messages = &requests[0].messages;
    assert_eq!(
        request_messages
            .iter()
            .map(|message| &message.role)
            .collect::<Vec<_>>(),
        vec![
            &ModelRole::Developer,
            &ModelRole::User,
            &ModelRole::Assistant,
            &ModelRole::User,
        ]
    );
    assert_eq!(
        request_messages
            .iter()
            .skip(1)
            .map(|message| message.content[0].text.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("previous user"),
            Some("previous assistant"),
            Some("current user"),
        ]
    );
    assert!(
        request_messages[0].content[0]
            .text
            .as_deref()
            .expect("developer instructions")
            .ends_with(project_instructions)
    );
}

#[test]
fn agent_loop_projects_registered_tools_to_provider_request() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello")
        .with_model_name(Some("gpt-test".to_string()));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests lock");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "builtin.read")
    );
    assert_eq!(
        requests[0].model_preferences.model_name.as_deref(),
        Some("gpt-test")
    );
    assert_eq!(
        requests[0].context_metadata["included_item_ids"],
        serde_json::json!(["input_1"])
    );
    assert_eq!(
        requests[0].context_metadata["budget"]["model_context_window"],
        serde_json::json!(DEFAULT_MAX_CONTEXT_TOKENS)
    );
    assert_eq!(requests[0].trace_metadata["turn_id"], "turn_1");
}

#[test]
fn agent_loop_executes_workspace_read_tool_with_safe_tool_result() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "hello from workspace").expect("write readme");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.read",
        serde_json::json!({"path": "README.md", "max_chars": 64}),
    ));

    let result = agent_loop_with_response(response, allow_read_policy())
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].ok);
    assert_eq!(result.error.as_deref(), Some("max turns exceeded"));
    let payload = result.tool_results[0].to_message_payload();
    let serialized = serde_json::to_string(&payload).expect("serialize payload");
    assert!(serialized.contains("README.md"));
    assert!(serialized.contains("hello from workspace"));
    assert!(!serialized.contains("raw_arguments"));
}

#[test]
fn agent_loop_rechecks_context_budget_before_each_model_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "small tool result").expect("write readme");
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "read the file")
    };
    let mut oversized_call = tool_call(
        "call_1",
        "builtin.read",
        serde_json::json!({"path": "README.md", "max_chars": 64}),
    );
    oversized_call.raw_arguments = serde_json::json!({
        "path": "README.md",
        "padding": "x".repeat(DEFAULT_MAX_CONTEXT_TOKENS as usize * 4)
    })
    .to_string();
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    tool_response.tool_calls.push(oversized_call);
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "must not run");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![tool_response, final_response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("model request exceeds the model context budget")
    );
    assert_eq!(result.model_turns, 1);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].ok);
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 1);
}

#[test]
fn agent_loop_approval_grant_allows_workspace_mutation_without_policy_reask() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput {
        max_turns: 3,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
            ApprovalGrant::allow("approval_turn_1_call_1", "builtin.edit", ["README.md"]),
        )
    };
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    tool_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut verification_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    verification_response.tool_calls.push(tool_call(
        "call_2",
        "builtin.command",
        serde_json::json!({"argv": ["cargo", "test"], "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![tool_response, verification_response, final_response],
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
            PermissionRule::new(
                "allow_execute",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute),
        ),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert!(result.error.is_none(), "error={:?}", result.error);
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.approval_count, 0);
    assert_eq!(result.model_turns, 3);
    assert!(result.approval_requests.is_empty());
    assert!(result.tool_results[0].ok);
    assert!(result.tool_results[1].ok);
    assert!(result.verification.required);
    assert!(result.verification.passed);
    assert_eq!(result.verification.successful_command_count, 1);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages[2].role, ModelRole::Assistant);
    assert_eq!(requests[1].messages[2].tool_calls.len(), 1);
    assert_eq!(requests[1].messages[2].tool_calls[0].tool_call_id, "call_1");
    assert_eq!(requests[1].messages[3].role, ModelRole::Tool);
    assert_eq!(
        requests[1].messages[3].tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn agent_loop_retries_model_after_repairable_workspace_tool_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput {
        max_turns: 4,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut failing_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    failing_tool_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "missing",
            "replacement": "after"
        }),
    ));
    let mut repaired_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_tool_response.tool_calls.push(tool_call(
        "call_2",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut verification_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    verification_response.tool_calls.push(tool_call(
        "call_3",
        "builtin.command",
        serde_json::json!({"argv": ["cargo", "test"], "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "allow_write",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write),
        )
        .with_rule(
            PermissionRule::new(
                "allow_execute",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute),
        );

    let result = agent_loop_with_responses_and_requests(
        vec![
            failing_tool_response,
            repaired_tool_response,
            verification_response,
            final_response,
        ],
        policy,
        seen_requests.clone(),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 4);
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(result.tool_repairs.len(), 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("expected_content_missing")
    );
    assert_eq!(
        result.tool_repairs[0].failed_tool_call_id.as_str(),
        "call_1"
    );
    assert_eq!(
        result.tool_repairs[0].failure_kind.as_str(),
        "expected_content_missing"
    );
    assert_eq!(
        result.tool_repairs[0].failed_result["error_code"],
        serde_json::json!("expected_content_missing")
    );
    assert_eq!(
        result.tool_repairs[0].repair_contract["retry_after_turn"],
        serde_json::json!(1)
    );
    assert!(result.tool_results[1].ok);
    assert!(result.tool_results[2].ok);
    assert!(result.verification.passed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].messages.last().unwrap().role, ModelRole::Tool);
    assert!(
        requests[1]
            .messages
            .last()
            .unwrap()
            .content
            .iter()
            .any(|block| block
                .text
                .as_deref()
                .is_some_and(|text| text.contains("expected_content_missing")))
    );
}

#[test]
fn agent_loop_command_fails_closed_without_sandbox_backend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command")
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": test_command("success"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_command",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );

    let result = agent_loop_with_responses_and_requests(
        vec![command_response, final_response],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.tool_results.len(), 1);
    assert!(!result.tool_results[0].ok);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("sandbox_unavailable")
    );
    let audit = result.tool_results[0]
        .audit_metadata()
        .expect("command audit metadata");
    assert_eq!(
        audit["command_scope_digest"],
        command_scope_digest(
            &test_command("success"),
            ".",
            5,
            &SandboxFilesystemMode::WorkspaceWrite,
            &SandboxNetworkMode::Denied,
        )
    );
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("sandbox")
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
}

#[test]
fn agent_loop_command_uses_strict_sandbox_backend_when_injected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command")
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": test_command("success"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_command",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );

    let result = agent_loop_with_responses_and_requests(
        vec![command_response, final_response],
        policy,
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 2);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].ok);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
}

#[test]
fn agent_loop_returns_command_nonzero_to_model_for_repair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput {
        max_turns: 3,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command")
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": test_command("failure"),
            "timeout_seconds": 5
        }),
    ));
    let mut repaired_command_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_command_response.tool_calls.push(tool_call(
        "call_2",
        "builtin.command",
        serde_json::json!({
            "argv": test_command("repaired"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "handled failure");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_command",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![command_response, repaired_command_response, final_response],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(
        AgentFailThenSucceedBackend {
            calls: AtomicUsize::new(0),
        },
    ))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("command_exit_nonzero")
    );
    assert_eq!(result.tool_repairs.len(), 1);
    assert!(result.tool_results[1].ok);
    assert!(result.verification.unresolved_failures.is_empty());
    assert_eq!(
        result.tool_repairs[0].failure_kind.as_str(),
        "command_exit_nonzero"
    );
    assert_eq!(result.final_answer.as_deref(), Some("handled failure"));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].messages.last().unwrap().role, ModelRole::Tool);
    assert_eq!(
        requests[1].messages.last().unwrap().tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn agent_loop_cancels_a_running_sandbox_command() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let mut command_response = ModelTurnResponse::completed("request_1", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({"argv": ["cargo", "test"], "timeout_seconds": 30}),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_execute",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );
    let workspace = WorkspaceTools::new(dir.path()).with_sandbox_backend(BlockingCommandBackend {
        started: Mutex::new(Some(started_tx)),
    });
    let worker = thread::spawn(move || {
        agent_loop_with_response(command_response, policy)
            .with_workspace_tools(workspace)
            .with_cancellation_token(worker_cancellation)
            .run(&AgentLoopInput::new("thread_1", "turn_1", "run command"))
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("sandbox command started");
    cancellation.cancel();
    let result = worker.join().expect("agent worker joins");

    assert_eq!(result.status, AgentStatus::Cancelled);
    assert!(!result.completed);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("command_cancelled")
    );
}

#[test]
fn agent_loop_approval_grant_cannot_override_denied_profile_network() {
    let dir = tempfile::tempdir().expect("temp dir");
    let argv = test_command("must-not-execute");
    let resource = command_scope_resource(
        &argv,
        ".",
        5,
        &SandboxFilesystemMode::ReadOnly,
        &SandboxNetworkMode::Allowed,
    );
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "run network command").with_approval_grant(
            ApprovalGrant::allow(
                "approval_turn_1_call_1",
                "builtin.command",
                [resource.clone()],
            ),
        )
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": argv,
            "network_access": "allowed",
            "timeout_seconds": 5
        }),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "allow_command",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource(resource.clone()),
        )
        .with_rule(
            PermissionRule::new(
                "allow_network",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Network)
            .for_resource(resource),
        );

    let result = agent_loop_with_response(response, policy)
        .with_workspace_tools(
            WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend),
        )
        .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_denied")
    );
    assert_eq!(result.approval_count, 0);
}

#[test]
fn agent_loop_command_approval_grant_requires_exact_command_resource() {
    let dir = tempfile::tempdir().expect("temp dir");
    let argv = test_command("success");
    let command_resource = command_scope_resource(
        &argv,
        ".",
        5,
        &SandboxFilesystemMode::WorkspaceWrite,
        &SandboxNetworkMode::Denied,
    );
    let mismatched_resource = command_scope_resource(
        &argv,
        ".",
        6,
        &SandboxFilesystemMode::WorkspaceWrite,
        &SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command").with_approval_grant(
            ApprovalGrant::allow(
                "approval_turn_1_call_1",
                "builtin.command",
                [mismatched_resource],
            ),
        )
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": argv,
            "timeout_seconds": 5
        }),
    ));

    let result = agent_loop_with_response(
        command_response,
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.approval_requests[0].resources,
        vec![command_resource]
    );
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    let pending = result.pending_tool_calls.first().expect("pending command");
    let pending_arguments: serde_json::Value =
        serde_json::from_str(&pending.raw_arguments).expect("pending arguments");
    assert_eq!(pending_arguments["cwd"], ".");
    assert_eq!(pending_arguments["timeout_seconds"], 5);
    assert_eq!(pending_arguments["sandbox_mode"], "workspace_write");
    assert_eq!(pending_arguments["network_access"], "denied");
}

#[test]
fn agent_loop_command_audit_records_sandbox_approval_and_provenance() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command")
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": test_command("success"),
            "sandbox_mode": "danger_full_access",
            "network_access": "allowed",
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let mut profile = PermissionProfile::workspace_write("C:/repo");
    profile.profile = PermissionProfileName::DangerFullAccess;
    profile.network_access = NetworkAccess::Allowed;
    let policy = PolicyEngine::new(profile)
        .with_rule(
            PermissionRule::new(
                "allow_command",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute),
        )
        .with_rule(
            PermissionRule::new(
                "allow_network",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Network),
        );

    let result = agent_loop_with_responses_and_requests(
        vec![command_response, final_response],
        policy,
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);
    let run_status = result.to_run_status(&input);

    assert_eq!(run_status.status, AgentStatus::Completed);
    assert_eq!(run_status.audit_events.len(), 1);
    assert_eq!(
        run_status.audit_events[0]["sandbox_mode"],
        "danger_full_access"
    );
    assert_eq!(run_status.audit_events[0]["cwd"], ".");
    assert_eq!(run_status.audit_events[0]["timeout_seconds"], 5);
    assert_eq!(run_status.audit_events[0]["network_access"], "allowed");
    assert_eq!(
        run_status.audit_events[0]["sandbox_backend"],
        "agent_strict_test"
    );
    assert_eq!(run_status.audit_events[0]["sandbox_enforcement"], "strict");
    assert_eq!(run_status.audit_events[0]["local_process_fallback"], false);
    assert_eq!(
        run_status.audit_events[0]["command_scope_digest"],
        command_scope_digest(
            &test_command("success"),
            ".",
            5,
            &SandboxFilesystemMode::DangerFullAccess,
            &SandboxNetworkMode::Allowed,
        )
    );
    assert_eq!(run_status.audit_events[0]["approval_policy"], "on-request");
    assert_eq!(
        run_status.audit_events[0]["approval_decision"],
        "allowed_by_policy"
    );
    assert_eq!(
        run_status.audit_events[0]["command_provenance"],
        "agent_requested"
    );
}

struct AgentStrictBackend;

impl SandboxBackend for AgentStrictBackend {
    fn name(&self) -> &'static str {
        "agent_strict_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "agent command ok").with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

struct BlockingCommandBackend {
    started: Mutex<Option<mpsc::Sender<()>>>,
}

impl SandboxBackend for BlockingCommandBackend {
    fn name(&self) -> &'static str {
        "blocking_command_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::backend_error(&request.command_id, "cancellable execution required")
    }

    fn execute_cancellable(
        &self,
        request: &CommandRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if let Some(started) = self.started.lock().expect("started lock").take() {
            started.send(()).expect("signal command start");
        }
        while !cancellation.is_cancelled() {
            thread::sleep(Duration::from_millis(5));
        }
        CommandResult::cancelled(&request.command_id, 1).with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

struct AgentFailThenSucceedBackend {
    calls: AtomicUsize,
}

impl SandboxBackend for AgentFailThenSucceedBackend {
    fn name(&self) -> &'static str {
        "agent_fail_then_succeed_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        } else {
            CommandResult::completed(&request.command_id, "repaired").with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
        }
    }
}

#[test]
fn agent_loop_approval_grant_matches_request_id_and_is_single_use() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "one").expect("write file");
    let input = AgentLoopInput {
        ..AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
            ApprovalGrant::allow("approval_turn_1_call_1", "builtin.edit", ["README.md"]),
        )
    };
    let mut first_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first_tool_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "one",
            "replacement": "two"
        }),
    ));
    let mut second_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    second_tool_response.tool_calls.push(tool_call(
        "call_2",
        "builtin.edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "two",
            "replacement": "three"
        }),
    ));

    let result = agent_loop_with_responses_and_requests(
        vec![first_tool_response, second_tool_response],
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.approval_requests[0].request_id,
        "approval_turn_1_call_2"
    );
    assert_eq!(
        result.tool_results[1].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "two"
    );
}

#[test]
fn agent_loop_approval_grant_does_not_override_sensitive_resource_deny() {
    let dir = tempfile::tempdir().expect("temp dir");
    for sensitive_path in [".env", ".azure/token", ".gnupg/private", "id_ecdsa"] {
        let target = dir.path().join(sensitive_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create sensitive parent");
        }
        std::fs::write(&target, "TOKEN=secret").expect("write sensitive file");
        let input = AgentLoopInput {
            max_turns: 1,
            ..AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
                ApprovalGrant::allow("approval_turn_1_call_1", "builtin.edit", [sensitive_path]),
            )
        };
        let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
        response.tool_calls.push(tool_call(
            "call_1",
            "builtin.edit",
            serde_json::json!({
                "path": sensitive_path,
                "expected": "TOKEN=secret",
                "replacement": "TOKEN=changed"
            }),
        ));

        let result = agent_loop_with_response(
            response,
            PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
        )
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

        assert_eq!(result.status, AgentStatus::Failed, "{sensitive_path}");
        assert_eq!(
            result.tool_results[0].error_code.as_deref(),
            Some("tool_denied"),
            "{sensitive_path}"
        );
        assert_eq!(
            std::fs::read_to_string(target).expect("read sensitive file"),
            "TOKEN=secret",
            "{sensitive_path}"
        );
        let payload = result.tool_results[0].to_message_payload();
        let serialized = serde_json::to_string(&payload).expect("serialize payload");
        assert!(!serialized.contains(sensitive_path));
        assert!(!serialized.contains("TOKEN=secret"));
    }
}

#[test]
fn agent_loop_patch_grant_does_not_override_sensitive_resource_deny() {
    let dir = tempfile::tempdir().expect("temp dir");
    let env_path = dir.path().join(".env");
    std::fs::write(&env_path, "TOKEN=secret").expect("write env");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
            ApprovalGrant::allow("approval_turn_1_call_1", "builtin.patch", [".env"]),
        )
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.patch",
        serde_json::json!({
            "changes": [{
                "path": ".env",
                "expected": "TOKEN=secret",
                "replacement": "TOKEN=changed"
            }]
        }),
    ));

    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_denied")
    );
    assert_eq!(
        std::fs::read_to_string(env_path).expect("read env"),
        "TOKEN=secret"
    );
    let payload = result.tool_results[0].to_message_payload();
    let serialized = serde_json::to_string(&payload).expect("serialize payload");
    assert!(!serialized.contains(".env"));
    assert!(!serialized.contains("TOKEN=secret"));
}

#[test]
fn agent_loop_patch_policy_checks_every_change_path_before_writing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first_path = dir.path().join("first.md");
    let second_path = dir.path().join("second.md");
    std::fs::write(&first_path, "one").expect("write first");
    std::fs::write(&second_path, "two").expect("write second");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.patch",
        serde_json::json!({
            "changes": [
                {
                    "path": "first.md",
                    "expected": "one",
                    "replacement": "changed one"
                },
                {
                    "path": "second.md",
                    "expected": "two",
                    "replacement": "changed two"
                }
            ]
        }),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo"))
        .with_rule(
            PermissionRule::new(
                "allow_first",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource("first.md"),
        )
        .with_rule(
            PermissionRule::new(
                "deny_second",
                SettingsScope::Project,
                PermissionDecisionOutcome::Deny,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource("second.md"),
        );

    let result = agent_loop_with_response(response, policy)
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_denied")
    );
    assert_eq!(
        std::fs::read_to_string(first_path).expect("read first"),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(second_path).expect("read second"),
        "two"
    );
}

#[test]
fn agent_loop_patch_approval_request_covers_unapproved_change_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first_path = dir.path().join("first.md");
    let second_path = dir.path().join("second.md");
    std::fs::write(&first_path, "one").expect("write first");
    std::fs::write(&second_path, "two").expect("write second");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.patch",
        serde_json::json!({
            "changes": [
                {
                    "path": "first.md",
                    "expected": "one",
                    "replacement": "changed one"
                },
                {
                    "path": "second.md",
                    "expected": "two",
                    "replacement": "changed two"
                }
            ]
        }),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_first",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write)
        .for_resource("first.md"),
    );

    let result = agent_loop_with_response(response, policy)
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        result.approval_requests[0].request_id,
        "approval_turn_1_call_1"
    );
    assert_eq!(result.approval_requests[0].thread_id, "thread_1");
    assert_eq!(result.approval_requests[0].turn_id, "turn_1");
    assert_eq!(
        result.approval_requests[0].tool_call_id.as_deref(),
        Some("call_1")
    );
    assert_eq!(
        std::fs::read_to_string(first_path).expect("read first"),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(second_path).expect("read second"),
        "two"
    );
}

#[test]
fn agent_loop_approval_grant_requires_exact_resource_set() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first_path = dir.path().join("first.md");
    let second_path = dir.path().join("second.md");
    std::fs::write(&first_path, "one").expect("write first");
    std::fs::write(&second_path, "two").expect("write second");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
            ApprovalGrant::allow("approval_turn_1_call_1", "builtin.patch", ["first.md"]),
        )
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.patch",
        serde_json::json!({
            "changes": [
                {
                    "path": "first.md",
                    "expected": "one",
                    "replacement": "changed one"
                },
                {
                    "path": "second.md",
                    "expected": "two",
                    "replacement": "changed two"
                }
            ]
        }),
    ));

    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        std::fs::read_to_string(first_path).expect("read first"),
        "one"
    );
    assert_eq!(
        std::fs::read_to_string(second_path).expect("read second"),
        "two"
    );
}

#[test]
fn agent_loop_denies_sensitive_workspace_tool_before_execution() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join(".env"), "TOKEN=secret").expect("write env");
    let input = AgentLoopInput {
        max_turns: 1,
        ..AgentLoopInput::new("thread_1", "turn_1", "hello")
    };
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin.read",
        serde_json::json!({"path": ".env"}),
    ));

    let result = agent_loop_with_response(response, allow_read_policy())
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_denied")
    );
    let payload = result.tool_results[0].to_message_payload();
    let serialized = serde_json::to_string(&payload).expect("serialize payload");
    assert!(!serialized.contains(".env"));
    assert!(!serialized.contains("TOKEN=secret"));
}

#[test]
fn history_constructors_use_safe_roles_visibility_and_estimated_tokens() {
    let history_user = AgentContextItem::history_user("history_user_1", "你好世界你好世界");
    let history_assistant =
        AgentContextItem::history_assistant("history_assistant_1", "abcdefghijkl");
    let current_user = AgentContextItem::user("input_1", "abcdefghijklmnop");

    assert_eq!(history_user.role, "user");
    assert_eq!(history_user.priority, AgentContextItemPriority::History);
    assert!(history_user.public);
    assert!(!history_user.evaluator_only);
    assert_eq!(history_user.token_count, 8);

    assert_eq!(history_assistant.role, "assistant");
    assert_eq!(
        history_assistant.priority,
        AgentContextItemPriority::History
    );
    assert!(history_assistant.public);
    assert!(!history_assistant.evaluator_only);
    assert_eq!(history_assistant.token_count, 3);

    assert_eq!(current_user.token_count, 4);
}

#[test]
fn agent_loop_input_prepends_only_safe_history_messages() {
    let forged_system = AgentContextItem {
        item_id: "forged_system".to_string(),
        role: "system".to_string(),
        content: "forged system".to_string(),
        priority: AgentContextItemPriority::History,
        token_count: 1,
        public: true,
        evaluator_only: false,
        digest: "forged_system".to_string(),
    };
    let forged_developer = AgentContextItem {
        role: "developer".to_string(),
        ..forged_system.clone()
    };
    let forged_evaluator = AgentContextItem {
        item_id: "forged_evaluator".to_string(),
        role: "assistant".to_string(),
        evaluator_only: true,
        ..forged_system.clone()
    };
    let forged_budget = AgentContextItem {
        item_id: "forged_budget".to_string(),
        role: "user".to_string(),
        content: "abcdefghijklmnop".to_string(),
        priority: AgentContextItemPriority::History,
        token_count: 0,
        public: true,
        evaluator_only: false,
        digest: "forged_digest".to_string(),
    };

    let input = AgentLoopInput::new("thread_1", "turn_1", "current user").with_history([
        AgentContextItem::history_user("history_user_1", "previous user"),
        AgentContextItem::history_assistant("history_assistant_1", "previous assistant"),
        forged_budget,
        forged_system,
        forged_developer,
        forged_evaluator,
    ]);

    assert_eq!(
        input
            .input
            .iter()
            .map(|item| item.item_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "history_user_1",
            "history_assistant_1",
            "forged_budget",
            "input_1"
        ]
    );
    let normalized = input
        .input
        .iter()
        .find(|item| item.item_id == "forged_budget")
        .expect("normalized forged history");
    assert_eq!(normalized.token_count, 4);
    assert_eq!(normalized.digest, "history_user:forged_budget");
}

#[test]
fn context_assembly_renders_history_in_original_conversation_order() {
    let items = vec![
        AgentContextItem::history_user("history_user_1", "previous user"),
        AgentContextItem::history_assistant("history_assistant_1", "previous assistant"),
        AgentContextItem::user("input_1", "current user"),
    ];

    let context = assemble_context_items(&items, 100);

    assert_eq!(
        context.included_item_ids,
        vec!["history_user_1", "history_assistant_1", "input_1"]
    );
    assert_eq!(context.messages[0]["role"], "user");
    assert_eq!(context.messages[1]["role"], "assistant");
    assert_eq!(context.messages[2]["role"], "user");
    assert_eq!(context.messages[0]["content"], "previous user");
    assert_eq!(context.messages[1]["content"], "previous assistant");
    assert_eq!(context.messages[2]["content"], "current user");
    assert_eq!(
        context.bundle_digest,
        "history_user:history_user_1:history_assistant:history_assistant_1:user_input"
    );
}

#[test]
fn context_assembly_keeps_the_newest_complete_history_turns() {
    let items = vec![
        AgentContextItem::history_user("old_user", "abcdefgh"),
        AgentContextItem::history_assistant("old_assistant", "ijklmnop"),
        AgentContextItem::history_user("new_user", "qrstuvwx"),
        AgentContextItem::history_assistant("new_assistant", "yzabcdef"),
        AgentContextItem::user("input_1", "ghijklmn"),
    ];

    let context = assemble_context_items(&items, 6);

    assert_eq!(
        context.included_item_ids,
        vec!["new_user", "new_assistant", "input_1"]
    );
    assert_eq!(context.excluded_item_ids, vec!["old_user", "old_assistant"]);
    assert_eq!(context.messages[0]["content"], "qrstuvwx");
    assert_eq!(context.messages[1]["content"], "yzabcdef");
    assert_eq!(context.messages[2]["content"], "ghijklmn");
}
#[test]
fn context_assembly_keeps_current_user_when_history_exceeds_budget() {
    let items = vec![
        AgentContextItem::history_user("history_user_1", "abcdefgh"),
        AgentContextItem::history_assistant("history_assistant_1", "ijklmnop"),
        AgentContextItem::user("input_1", "qrstuvwx"),
    ];

    let context = assemble_context_items(&items, 2);

    assert_eq!(context.included_item_ids, vec!["input_1"]);
    assert_eq!(
        context.excluded_item_ids,
        vec!["history_user_1", "history_assistant_1"]
    );
    assert_eq!(context.messages.len(), 1);
    assert_eq!(context.messages[0]["content"], "qrstuvwx");
    assert_eq!(context.budget["message_tokens"], 2);
}

#[test]
fn context_assembly_keeps_user_turn_and_safe_tool_results_with_budget() {
    let items = vec![
        AgentContextItem {
            item_id: "tool_raw".to_string(),
            role: "tool".to_string(),
            content: "raw".to_string(),
            priority: AgentContextItemPriority::Evidence,
            token_count: 3,
            public: false,
            evaluator_only: false,
            digest: "digest_raw".to_string(),
        },
        AgentContextItem {
            item_id: "user_1".to_string(),
            role: "user".to_string(),
            content: "fix tests".to_string(),
            priority: AgentContextItemPriority::CurrentTurn,
            token_count: 6,
            public: true,
            evaluator_only: false,
            digest: "digest_user".to_string(),
        },
        AgentContextItem {
            item_id: "eval_1".to_string(),
            role: "system".to_string(),
            content: "hidden scorer".to_string(),
            priority: AgentContextItemPriority::System,
            token_count: 4,
            public: true,
            evaluator_only: true,
            digest: "digest_eval".to_string(),
        },
        AgentContextItem {
            item_id: "tool_safe".to_string(),
            role: "tool".to_string(),
            content: "safe preview".to_string(),
            priority: AgentContextItemPriority::Evidence,
            token_count: 5,
            public: true,
            evaluator_only: false,
            digest: "digest_tool".to_string(),
        },
    ];

    let context = assemble_context_items(&items, 11);

    assert_eq!(context.included_item_ids, vec!["user_1", "tool_safe"]);
    assert_eq!(context.excluded_item_ids, vec!["tool_raw", "eval_1"]);
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0]["role"], "user");
    assert_eq!(context.messages[1]["role"], "tool");
    assert_eq!(context.budget["message_tokens"], 11);
    assert!(context.bundle_digest.contains("digest_user"));
    assert!(context.bundle_digest.contains("digest_tool"));
}

#[test]
fn agent_loop_uses_shared_model_context_token_limit() {
    let items = vec![AgentContextItem {
        item_id: "large_user".to_string(),
        role: "user".to_string(),
        content: "large request".to_string(),
        priority: AgentContextItemPriority::CurrentTurn,
        token_count: DEFAULT_MAX_CONTEXT_TOKENS + 1,
        public: true,
        evaluator_only: false,
        digest: "digest_large".to_string(),
    }];

    let context = assemble_context_items(&items, DEFAULT_MAX_CONTEXT_TOKENS);

    assert!(context.included_item_ids.is_empty());
    assert_eq!(context.excluded_item_ids, vec!["large_user"]);
}

#[test]
fn agent_loop_fails_closed_before_provider_when_current_turn_exceeds_context_budget() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("req_1", "resp_1", "should not be used"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    );
    let oversized = "a".repeat(DEFAULT_MAX_CONTEXT_TOKENS as usize * 4 + 1);
    let input = AgentLoopInput::new("thread_1", "turn_1", oversized);

    let result = agent_loop.run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("current turn exceeds the model context budget")
    );
    assert_eq!(result.model_turns, 0);
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_reserves_the_requested_model_output_budget() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("req_1", "resp_1", "should not be used"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    );
    let mut input = AgentLoopInput::new("thread_1", "turn_1", "current user");
    input.model_preferences.max_output_tokens = Some(DEFAULT_MAX_CONTEXT_TOKENS);

    let result = agent_loop.run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("current turn exceeds the model context budget")
    );
    assert_eq!(result.model_turns, 0);
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}
#[test]
fn agent_status_mapping_preserves_blocked_and_cancelled() {
    assert_eq!(AgentStatus::from("blocked"), AgentStatus::Blocked);
    assert_eq!(AgentStatus::from("cancelled"), AgentStatus::Cancelled);
    assert_eq!(AgentStatus::from("max_turns_exceeded"), AgentStatus::Failed);
}
