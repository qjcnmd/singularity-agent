use singularity_agent::{
    AgentContextItem, AgentContextItemPriority, AgentLoop, AgentLoopCapability, AgentLoopInput,
    AgentLoopStep, AgentRunStatus, AgentStatus, ApprovalGrant, CompletionGateInput, ContextBundle,
    ContextSummaryEnvelope, EvaluationDiagnostics, EvaluationRunReport, FinalReportMapping,
    PlannerNextAction, PlannerState, PythonSidecarClient, PythonSidecarConfig,
    PythonSidecarRunResult, RepairNextAction, SidecarRunEvent, ToolRepair, assemble_context_items,
    completion_gate_allows_final, final_mapping_from_status, planner_next_action,
    repair_next_action, sidecar_trace_summary,
};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, Provider, ProviderError,
};
use singularity_policy::{
    PermissionDecisionOutcome, PermissionOperation, PermissionProfile, PermissionRule,
    PolicyEngine, SettingsScope,
};
use singularity_tools::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus, SandboxBackend,
    SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolRegistry,
    ToolSpec, WorkspaceTools, command_scope_resource,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TEST_SIDECAR_RESPONSE_TIMEOUT_MS: u64 = 100;

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
        provider_metadata: serde_json::json!({}),
    }
}

fn python_command(code: &str) -> Vec<String> {
    vec![python_bin(), "-c".to_string(), code.to_string()]
}

fn python_bin() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
}

#[test]
fn agent_run_status_reports_not_migrated_without_claiming_completion() {
    let run_status = AgentRunStatus::not_migrated();

    assert_eq!(run_status.status, AgentStatus::NotMigrated);
    assert!(!run_status.completed);
    assert_eq!(run_status.status.as_str(), "not_migrated");
    assert!(run_status.final_answer.is_none());
}

#[test]
fn agent_loop_capability_is_available_without_remaining_blockers() {
    let capability = AgentLoopCapability::current();

    assert!(capability.available);
    assert_eq!(capability.status, AgentStatus::Completed);
    assert!(capability.reason.contains("native Rust AgentLoop"));
    assert!(capability.blockers.is_empty());
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
fn agent_loop_final_answer_completes_without_completion_gate_scaffold() {
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
fn agent_loop_approval_grant_allows_workspace_mutation_without_policy_reask() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput {
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
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![tool_response, final_response],
        PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert!(result.error.is_none(), "error={:?}", result.error);
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.approval_count, 0);
    assert_eq!(result.model_turns, 2);
    assert!(result.approval_requests.is_empty());
    assert!(result.tool_results[0].ok);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages[1].role, ModelRole::Assistant);
    assert_eq!(requests[1].messages[1].tool_calls.len(), 1);
    assert_eq!(requests[1].messages[1].tool_calls[0].tool_call_id, "call_1");
    assert_eq!(requests[1].messages[2].role, ModelRole::Tool);
    assert_eq!(
        requests[1].messages[2].tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn agent_loop_retries_model_after_repairable_workspace_tool_failure() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput {
        max_turns: 3,
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
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write("C:/repo")).with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = agent_loop_with_responses_and_requests(
        vec![
            failing_tool_response,
            repaired_tool_response,
            final_response,
        ],
        policy,
        seen_requests.clone(),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.tool_results.len(), 2);
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
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
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
            "argv": python_command("print('command ok')"),
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
            "argv": python_command("print('command ok')"),
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
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command")
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "builtin.command",
        serde_json::json!({
            "argv": python_command("raise SystemExit(1)"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "handled failure");
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
        vec![command_response, final_response],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentNonzeroBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 2);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("command_exit_nonzero")
    );
    assert_eq!(result.tool_repairs.len(), 1);
    assert_eq!(
        result.tool_repairs[0].failure_kind.as_str(),
        "command_exit_nonzero"
    );
    assert_eq!(result.final_answer.as_deref(), Some("handled failure"));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].messages[2].role, ModelRole::Tool);
    assert_eq!(
        requests[1].messages[2].tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn agent_loop_command_approval_grant_requires_exact_command_resource() {
    let dir = tempfile::tempdir().expect("temp dir");
    let argv = python_command("print('command ok')");
    let command_resource = command_scope_resource(
        &argv,
        &SandboxFilesystemMode::ReadOnly,
        &SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput {
        max_turns: 2,
        ..AgentLoopInput::new("thread_1", "turn_1", "run command").with_approval_grant(
            ApprovalGrant::allow(
                "approval_turn_1_call_1",
                "builtin.command",
                ["builtin.command"],
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
        CommandResult::completed(&request.command_id, "agent command ok")
    }
}

struct AgentNonzeroBackend;

impl SandboxBackend for AgentNonzeroBackend {
    fn name(&self) -> &'static str {
        "agent_nonzero_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult {
            command_id: request.command_id.clone(),
            execution_status: CommandExecutionStatus::Completed,
            semantic_status: CommandSemanticStatus::ExitNonzero,
            exit_code: Some(1),
            duration_ms: 1,
            timed_out: false,
            stdout_preview: String::new(),
            stderr_preview: "command failed".to_string(),
            output_truncated: false,
            redacted: true,
            changed_files: Vec::new(),
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
fn evaluation_report_contract_keeps_gate_fields_separate_from_diagnostics() {
    let report = EvaluationRunReport {
        evaluation_passed: false,
        agent_completed: true,
        tests_passed: true,
        public_verification_passed: true,
        hidden_verification_passed: false,
        local_process_fallback_count: 0,
        diagnostics: EvaluationDiagnostics {
            base_verification_passed: Some(false),
            sandbox_required: true,
            notes: vec!["diagnostic-only timing note".to_string()],
        },
    };

    let value = serde_json::to_value(&report).expect("serialize evaluation report");

    assert_eq!(value["evaluation_passed"], false);
    assert_eq!(value["agent_completed"], true);
    assert_eq!(value["tests_passed"], true);
    assert_eq!(value["public_verification_passed"], true);
    assert_eq!(value["hidden_verification_passed"], false);
    assert_eq!(value["local_process_fallback_count"], 0);
    assert_eq!(value["diagnostics"]["base_verification_passed"], false);
    assert_eq!(value["diagnostics"]["sandbox_required"], true);
    assert!(value["diagnostics"].get("evaluation_passed").is_none());
    assert!(value.get("base_verification_passed").is_none());

    let round_trip: EvaluationRunReport =
        serde_json::from_value(value).expect("deserialize evaluation report");
    assert_eq!(round_trip, report);
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
fn planner_repair_completion_and_final_mapping_are_deterministic() {
    let pending_approval = PlannerState {
        task_id: "task_1".to_string(),
        current_phase: "running_verification".to_string(),
        status: "running".to_string(),
        current_plan: Vec::new(),
        completion_criteria: serde_json::json!({}),
        open_actions: vec![serde_json::json!({"kind": "approval", "status": "pending"})],
        blocked_actions: Vec::new(),
        risk_escalations: Vec::new(),
        evidence_refs: Vec::new(),
    };
    let pending_tool = PlannerState {
        open_actions: vec![serde_json::json!({"kind": "tool", "status": "pending"})],
        ..pending_approval.clone()
    };
    let repair = ToolRepair {
        repair_id: "repair_1".to_string(),
        run_id: "run_1".to_string(),
        session_id: "session_1".to_string(),
        task_id: "task_1".to_string(),
        phase_id: "repairing_failures".to_string(),
        failed_tool_call_id: "call_1".to_string(),
        failure_kind: "tool_executor_failed".to_string(),
        next_action: "repair_then_verify".to_string(),
        failed_result: serde_json::json!({"ok": false}),
        recovery_report: serde_json::json!({}),
        repair_contract: serde_json::json!({}),
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        metadata: serde_json::json!({}),
    };

    assert_eq!(
        planner_next_action(&pending_approval),
        PlannerNextAction::ResumePendingApproval
    );
    assert_eq!(
        planner_next_action(&pending_tool),
        PlannerNextAction::ExecutePendingTool
    );
    assert_eq!(
        repair_next_action(&repair),
        RepairNextAction::RepairThenVerify
    );
    assert!(!completion_gate_allows_final(&CompletionGateInput {
        verification_passed: false,
        unresolved_failures: Vec::new(),
        interrupted: false,
    }));

    let mapping = final_mapping_from_status(
        "mapping_1",
        "run_1",
        "session_1",
        "task_1",
        AgentStatus::Completed,
        "done",
    );

    assert_eq!(mapping.run_status, "completed");
    assert_eq!(mapping.final_report_status, "completed");
    assert_eq!(mapping.completion_status, "completed");
    assert_eq!(mapping.final_answer, "done");
}

#[test]
fn sidecar_result_maps_agent_loop_status_without_raw_payloads() {
    let result = PythonSidecarRunResult {
        run_id: "run_1".to_string(),
        session_id: "session_1".to_string(),
        task_id: "task_1".to_string(),
        status: "completed".to_string(),
        final_answer: Some("done".to_string()),
        trace_path: Some("run_1".to_string()),
        events: vec![SidecarRunEvent {
            event_id: "event_1".to_string(),
            event_type: "lifecycle.run.started".to_string(),
            summary: "started".to_string(),
            component: "kernel".to_string(),
            severity: "info".to_string(),
            sequence: 0,
        }],
    };

    let run_status = AgentRunStatus::from_sidecar(result);
    let summary = sidecar_trace_summary(&run_status);

    assert_eq!(run_status.status, AgentStatus::Completed);
    assert!(run_status.completed);
    assert_eq!(run_status.final_answer.as_deref(), Some("done"));
    assert_eq!(summary["component"], "python_sidecar");
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["model_turns"], 0);
    assert_eq!(summary["trace_path"], "run_1");
    assert!(summary.get("raw_prompt").is_none());
}

#[test]
fn sidecar_result_ignores_raw_payload_and_metadata_fields() {
    let value = serde_json::json!({
        "run_id": "run_1",
        "session_id": "session_1",
        "task_id": "task_1",
        "status": "completed",
        "final_answer": "done",
        "trace_path": "run_1",
        "events": [
            {
                "event_id": "event_1",
                "event_type": "lifecycle.run.started",
                "summary": "started",
                "component": "kernel",
                "severity": "info",
                "sequence": 0,
                "raw_prompt": "do not project",
                "raw_response": "do not project",
                "raw_arguments": {"path": ".env"},
                "provider_response": {"token": "secret"},
                "metadata": {"api_key": "secret"}
            }
        ],
        "raw_prompt": "do not project",
        "raw_response": "do not project",
        "raw_arguments": {"path": ".env"},
        "provider_response": {"token": "secret"},
        "metadata": {"api_key": "secret"}
    });

    let result: PythonSidecarRunResult =
        serde_json::from_value(value).expect("unknown sidecar fields are ignored");
    let run_status = AgentRunStatus::from_sidecar(result);
    let summary = sidecar_trace_summary(&run_status);
    let summary_text = summary.to_string().to_lowercase();

    assert_eq!(run_status.status, AgentStatus::Completed);
    assert_eq!(run_status.events.len(), 1);
    for marker in [
        "raw_prompt",
        "raw_response",
        "raw_arguments",
        "provider_response",
        "metadata",
        "api_key",
        "token",
        "secret",
    ] {
        assert!(
            !summary_text.contains(marker),
            "sidecar trace summary leaked {marker}: {summary_text}"
        );
    }
}

#[test]
fn sidecar_status_mapping_preserves_blocked_and_cancelled() {
    assert_eq!(AgentStatus::from("blocked"), AgentStatus::Blocked);
    assert_eq!(AgentStatus::from("cancelled"), AgentStatus::Cancelled);
    assert_eq!(AgentStatus::from("max_turns_exceeded"), AgentStatus::Failed);
}

#[test]
fn sidecar_startup_failure_is_reported_without_fallback() {
    let config = PythonSidecarConfig {
        python_bin: "definitely_missing_python_sidecar_binary".to_string(),
        module: "singularity.agent_host.sidecar".to_string(),
        project_root: std::env::current_dir().expect("cwd"),
        python_path: None,
        env: Vec::new(),
    };

    let error = PythonSidecarClient::spawn(&config).expect_err("sidecar spawn should fail");

    assert!(error.contains("failed to start Python sidecar"));
}

#[test]
fn sidecar_cancel_and_status_return_typed_safe_envelopes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_status.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/cancel":
        result = {
            "run_id": params["runId"],
            "status": "cancel_requested",
            "raw_prompt": "do not project",
        }
    elif method == "agent/status":
        result = {
            "run_id": params["runId"],
            "status": "running",
            "raw_response": "do not project",
        }
    else:
        result = {"run_id": "unexpected", "status": "failed"}
    print(json.dumps({"id": message["id"], "result": result}), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_cancel_status".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let cancel = client.cancel("run_1").expect("cancel");
    let status = client.status("run_1").expect("status");

    assert_eq!(cancel.run_id, "run_1");
    assert_eq!(cancel.status, "cancel_requested");
    assert_eq!(status.run_id, "run_1");
    assert_eq!(status.status, "running");
}

#[test]
fn sidecar_cancel_status_reject_malformed_response() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_malformed_cancel.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({"id": message["id"], "result": {"status": "running"}}), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_malformed_cancel".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let error = client
        .cancel("run_1")
        .expect_err("missing run_id should be invalid");

    assert!(error.contains("invalid sidecar cancel result"));
}

#[test]
fn sidecar_status_reports_stdout_eof_as_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_eof.py"),
        "import sys\nsys.exit(0)\n",
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_eof".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let error = client
        .status("run_1")
        .expect_err("stdout EOF should be reported");

    assert!(
        error.contains("Python sidecar closed stdout")
            || error.contains("Python sidecar exited before response"),
        "unexpected sidecar EOF error: {error}"
    );
}

#[test]
fn sidecar_status_times_out_and_terminates_hung_sidecar() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_hang.py"),
        r#"
import json
import sys
import threading

for line in sys.stdin:
    json.loads(line)
    threading.Event().wait()
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_hang".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn_with_response_timeout(
        &config,
        Duration::from_millis(TEST_SIDECAR_RESPONSE_TIMEOUT_MS),
    )
    .expect("spawn sidecar");

    let error = client
        .status("run_1")
        .expect_err("hung sidecar should time out");

    assert!(error.contains("timed out waiting for Python sidecar response"));
}

#[test]
fn planner_state_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let planner: PlannerState =
        serde_json::from_value(fixture["planner_state"].clone()).expect("planner state");

    assert_eq!(planner.task_id, "task_1");
    assert_eq!(planner.current_phase, "running_verification");
    assert_eq!(planner.status, "repairing_failures");
    assert_eq!(planner.evidence_refs, vec!["obs_1"]);

    assert_eq!(
        serde_json::from_value::<PlannerState>(
            serde_json::to_value(&planner).expect("serialize planner")
        )
        .expect("deserialize planner"),
        planner
    );
}

#[test]
fn context_bundle_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let context: ContextBundle =
        serde_json::from_value(fixture["context_bundle"].clone()).expect("context bundle");

    assert_eq!(context.bundle_id, "bundle_1");
    assert_eq!(context.phase_id, "running_verification");
    assert_eq!(context.included_item_ids, vec!["item_goal", "item_plan"]);
    assert_eq!(context.excluded_item_ids, vec!["item_raw_tool"]);
    assert_eq!(context.budget["model_context_window"], 128000);
    assert_eq!(context.budget["message_tokens"], 62);
    assert_eq!(context.render_policy["include_raw_tool_outputs"], false);
    assert_eq!(context.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<ContextBundle>(
            serde_json::to_value(&context).expect("serialize context")
        )
        .expect("deserialize context"),
        context
    );
}

#[test]
fn context_summary_envelope_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let summary: ContextSummaryEnvelope =
        serde_json::from_value(fixture["context_summary_envelope"].clone())
            .expect("context summary envelope");

    assert_eq!(summary.version, 1);
    assert_eq!(summary.summary_id, "summary_1");
    assert_eq!(summary.source_item_ids, vec!["item_raw_tool"]);
    assert_eq!(summary.summary_payload["verification_status"], "passed");
    assert_eq!(
        summary.summary_payload["omitted_item_ids"],
        serde_json::json!(["item_raw_tool"])
    );
    assert_eq!(summary.cache_attribution["source"], "component_inferred");
    assert_eq!(summary.metadata["source"], "python_oracle");
    assert!(summary.rendered_summary.contains("verification=passed"));

    assert_eq!(
        serde_json::from_value::<ContextSummaryEnvelope>(
            serde_json::to_value(&summary).expect("serialize context summary")
        )
        .expect("deserialize context summary"),
        summary
    );
}

#[test]
fn tool_repair_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let repair: ToolRepair =
        serde_json::from_value(fixture["tool_repair"].clone()).expect("tool repair");

    assert_eq!(repair.repair_id, "tool_repair_1");
    assert_eq!(repair.failed_tool_call_id, "call_failed_1");
    assert_eq!(repair.failure_kind, "tool_executor_failed");
    assert_eq!(repair.next_action, "repair_then_verify");
    assert_eq!(repair.failed_result["ok"], false);
    assert_eq!(
        repair.recovery_report["succeeded_but_not_appended_call_ids"],
        serde_json::json!(["call_failed_1"])
    );
    assert_eq!(
        repair.repair_contract["allowed_tool_names"],
        serde_json::json!(["apply_patch", "read_file", "run_verification"])
    );
    assert_eq!(
        repair.repair_contract["verification_contract"]["contract_id"],
        "verification_contract_1"
    );
    assert_eq!(repair.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<ToolRepair>(
            serde_json::to_value(&repair).expect("serialize tool repair")
        )
        .expect("deserialize tool repair"),
        repair
    );
}

#[test]
fn final_report_mapping_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let mapping: FinalReportMapping =
        serde_json::from_value(fixture["final_report_mapping"].clone())
            .expect("final report mapping");

    assert_eq!(mapping.mapping_id, "finalization_mapping_1");
    assert_eq!(mapping.phase_id, "finalizing");
    assert_eq!(mapping.agent_loop_status, "completed");
    assert_eq!(mapping.run_status, "completed");
    assert_eq!(mapping.final_report_status, "completed");
    assert_eq!(mapping.completion_status, "completed");
    assert_eq!(
        mapping.final_report["verification_summary"]["status"],
        "ready"
    );
    assert_eq!(
        mapping.completion_assessment["unmet"],
        serde_json::json!([])
    );
    assert_eq!(mapping.contract_satisfaction["satisfied"], true);
    assert!(mapping.final_answer.contains("verification: ready"));
    assert_eq!(mapping.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<FinalReportMapping>(
            serde_json::to_value(&mapping).expect("serialize finalization mapping")
        )
        .expect("deserialize finalization mapping"),
        mapping
    );
}
