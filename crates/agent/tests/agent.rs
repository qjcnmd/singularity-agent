//! AgentLoop 的 Direct tool、completion、approval 和恢复回归测试。

use singularity_agent::{
    AgentContextItem, AgentContextItemPriority, AgentLoop, AgentLoopInput, AgentPlan,
    AgentPlanStep, AgentPlanStepStatus, AgentPlanUpdateInput, AgentRecoveryMetrics, AgentStatus,
    AgentVerificationRequirement, ApprovalGrant, agent_control_tool_entries,
    assemble_context_items,
};
use singularity_core::{CancellationToken, ProjectInstructions, load_project_instructions};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, ModelError, ModelErrorCategory, ModelErrorKind, ModelPreferences,
    ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse,
    ModelTurnStatus, ModelUsage, Provider, ProviderApiProtocol, ProviderAttemptMetadata,
    ProviderCapabilityMetadata, ProviderCapabilityProfile, ProviderError, ProviderProtocolContract,
    ProviderProtocolNegotiation, ToolChoiceMode,
};
use singularity_policy::{
    CommandScopeDigest, NetworkAccess, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionResource, PermissionRule, PolicyEngine, SettingsScope, ToolId,
    WorkspaceRelativePath,
};
use singularity_tools::{
    CommandRequest, CommandResult, CommandScriptRequest, SandboxBackend, SandboxCapabilities,
    SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolFailureKind, ToolRegistry,
    WorkspaceTools, command_script_scope_digest_with_policy, workspace_tool_entries,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("valid tool id")
}

fn workspace_resource(value: &str) -> PermissionResource {
    PermissionResource::WorkspacePath(
        WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
    )
}

fn typed_command_resource(digest: String) -> PermissionResource {
    PermissionResource::CommandScope(CommandScopeDigest::new(digest).expect("valid command digest"))
}

struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
}

struct FinalizationAwareProvider {
    setup_responses: Vec<ModelTurnResponse>,
    repeated_tool_response: ModelTurnResponse,
    final_response: Result<ModelTurnResponse, ProviderError>,
    cancel_on_finalization: bool,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
}

fn project_instruction_snapshot(content: &str) -> ProjectInstructions {
    let workspace = tempfile::tempdir().expect("project instruction workspace");
    std::fs::write(workspace.path().join("AGENTS.md"), content)
        .expect("write project instructions");
    load_project_instructions(workspace.path(), workspace.path())
        .expect("load project instructions")
        .expect("project instructions present")
}

impl Provider for FinalizationAwareProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.capabilities.clone()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        if let Some(response) = self.setup_responses.get(response_index) {
            return Ok(response.clone());
        }
        if request.tool_choice.mode == ToolChoiceMode::None && request.tools.is_empty() {
            if self.cancel_on_finalization {
                cancellation.cancel();
            }
            return self.final_response.clone();
        }
        Ok(self.repeated_tool_response.clone())
    }
}

impl Provider for StaticProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.capabilities.clone()
    }

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

struct NegotiatingProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    negotiation_calls: Arc<AtomicUsize>,
    static_capabilities: ProviderProtocolContract,
    negotiated_capabilities: Result<ProviderProtocolNegotiation, ProviderError>,
}

impl Provider for NegotiatingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.static_capabilities.clone()
    }

    fn negotiate_tool_capabilities(
        &self,
        _model_preferences: &ModelPreferences,
        _cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        self.negotiation_calls.fetch_add(1, Ordering::SeqCst);
        self.negotiated_capabilities.clone()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        Ok(self
            .responses
            .get(response_index)
            .unwrap_or_else(|| {
                self.responses
                    .last()
                    .expect("negotiating provider response")
            })
            .clone())
    }
}

fn negotiated_capability_metadata() -> ProviderCapabilityMetadata {
    ProviderCapabilityMetadata {
        api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
        profile: ProviderCapabilityProfile::StrictParallel,
        cache_hit: false,
        profile_attempts: 2,
        fallback_count: 1,
        probe_usage: ModelUsage {
            input_tokens: 3,
            output_tokens: 2,
            total_tokens: 5,
            ..ModelUsage::default()
        },
        probe_attempt_metadata: ProviderAttemptMetadata {
            attempt_count: 2,
            retry_count: 1,
            latency_ms: 7,
        },
    }
}

struct BlockingProvider {
    started: Mutex<Option<mpsc::Sender<()>>>,
}

impl Provider for BlockingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

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
    agent_loop_with_capabilities(
        responses,
        policy,
        seen_requests,
        ProviderProtocolContract::default(),
    )
}

fn agent_loop_with_capabilities(
    responses: Vec<ModelTurnResponse>,
    policy: PolicyEngine,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
) -> AgentLoop<StaticProvider> {
    agent_loop_with_capabilities_and_plan(responses, policy, seen_requests, capabilities, false)
}

fn agent_loop_with_plan_capabilities(
    responses: Vec<ModelTurnResponse>,
    policy: PolicyEngine,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
) -> AgentLoop<StaticProvider> {
    agent_loop_with_capabilities_and_plan(responses, policy, seen_requests, capabilities, true)
}

fn agent_loop_with_capabilities_and_plan(
    responses: Vec<ModelTurnResponse>,
    policy: PolicyEngine,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
    include_plan: bool,
) -> AgentLoop<StaticProvider> {
    AgentLoop::new(
        StaticProvider {
            responses,
            seen_requests,
            capabilities,
        },
        agent_tool_broker_for_test(include_plan),
        policy,
    )
    .with_workspace_tools(WorkspaceTools::new(env!("CARGO_MANIFEST_DIR")))
}

fn agent_tool_broker_for_test(include_plan: bool) -> ToolBroker {
    let mut registry = ToolRegistry::default();
    for entry in workspace_tool_entries()
        .into_iter()
        .filter(|entry| ["read", "edit", "patch", "command"].contains(&entry.spec.name.as_str()))
    {
        registry.register(entry).expect("register workspace tool");
    }
    for entry in agent_control_tool_entries() {
        if include_plan {
            registry
                .register(entry)
                .expect("register agent control tool");
        }
    }
    ToolBroker::new(registry)
}

fn allow_read_policy() -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_read",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Read),
    )
}

fn allow_read_execute_policy() -> PolicyEngine {
    allow_read_policy().with_rule(
        PermissionRule::new(
            "allow_execute",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    )
}

fn workspace_tool_broker_for_test() -> ToolBroker {
    agent_tool_broker_for_test(false)
}

fn allow_read_write_policy() -> PolicyEngine {
    allow_read_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    )
}

fn failed_model_response(error: ModelError) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed("request_1", "response_1", "unused");
    response.status = ModelTurnStatus::Failed;
    response.assistant_message = None;
    response.error = Some(error);
    response
}

#[test]
fn agent_loop_uses_negotiated_parallel_capability_and_keeps_optional_tools_non_strict() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let negotiation_calls = Arc::new(AtomicUsize::new(0));
    let static_capabilities = ProviderProtocolContract {
        supports_strict_tool_schema: false,
        supports_parallel_tool_calls: false,
        ..ProviderProtocolContract::default()
    };
    let negotiated_contract = ProviderProtocolContract {
        supports_strict_tool_schema: true,
        supports_parallel_tool_calls: true,
        ..ProviderProtocolContract::default()
    };
    let metadata = negotiated_capability_metadata();
    let agent_loop = AgentLoop::new(
        NegotiatingProvider {
            responses: vec![ModelTurnResponse::completed(
                "model_request_turn_1_0",
                "response_1",
                "done",
            )],
            seen_requests: Arc::clone(&seen_requests),
            negotiation_calls: Arc::clone(&negotiation_calls),
            static_capabilities,
            negotiated_capabilities: Ok(ProviderProtocolNegotiation {
                contract: negotiated_contract.clone(),
                metadata: metadata.clone(),
            }),
        },
        workspace_tool_broker_for_test(),
        allow_read_policy(),
    );

    let result = agent_loop.run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(negotiation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.provider_protocol_contract, Some(negotiated_contract));
    assert_eq!(result.provider_capability_metadata, Some(metadata));
    let requests = seen_requests.lock().expect("seen requests lock");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_choice.max_tool_calls, 8);
    assert!(!requests[0].tool_choice.strict_tool_schema);
    let serialized = serde_json::to_value(&result).expect("serialize result");
    assert!(serialized.get("provider_protocol_contract").is_none());
    assert!(serialized.get("provider_capability_metadata").is_none());
    let serialized_status =
        serde_json::to_value(result.to_run_status()).expect("serialize run status");
    assert!(
        serialized_status
            .get("provider_protocol_contract")
            .is_none()
    );
    assert!(
        serialized_status
            .get("provider_capability_metadata")
            .is_none()
    );
}

#[test]
fn capability_negotiation_failure_and_typed_cancel_skip_model_and_tool_execution() {
    for (kind, expected_status) in [
        (ModelErrorKind::UnsupportedCapability, AgentStatus::Failed),
        (ModelErrorKind::Cancelled, AgentStatus::Cancelled),
    ] {
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let negotiation_calls = Arc::new(AtomicUsize::new(0));
        let metadata = negotiated_capability_metadata();
        let error = ProviderError::from_model_error(
            ModelError::new(kind.clone(), "capability negotiation failed")
                .with_provider_diagnostic(
                    "capability_negotiation_failed",
                    singularity_model::ProviderErrorStage::ResponseValidation,
                ),
        )
        .with_capability_metadata(metadata.clone());
        let agent_loop = AgentLoop::new(
            NegotiatingProvider {
                responses: vec![ModelTurnResponse::completed(
                    "request_1",
                    "response_1",
                    "must not be used",
                )],
                seen_requests: Arc::clone(&seen_requests),
                negotiation_calls: Arc::clone(&negotiation_calls),
                static_capabilities: ProviderProtocolContract::default(),
                negotiated_capabilities: Err(error),
            },
            workspace_tool_broker_for_test(),
            allow_read_policy(),
        );

        let result = agent_loop.run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

        assert_eq!(result.status, expected_status);
        assert_eq!(result.model_turns, 0);
        assert_eq!(result.tool_calls, 0);
        assert_eq!(negotiation_calls.load(Ordering::SeqCst), 1);
        assert!(seen_requests.lock().expect("seen requests lock").is_empty());
        assert_eq!(result.provider_capability_metadata, Some(metadata));
        if expected_status == AgentStatus::Failed {
            assert_eq!(
                result.error_category,
                Some(ModelErrorCategory::UnsupportedCapability)
            );
            assert!(result.provider_diagnostic.is_some());
        }
    }
}

#[test]
fn approval_resume_re_negotiates_instead_of_using_checkpoint_capabilities() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file_path = workspace.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let mut edit_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    edit_response.tool_calls.push(tool_call(
        "edit_call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut verify_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    verify_response.tool_calls.push(tool_call(
        "verify_call_1",
        "command",
        serde_json::json!({"command": test_command_script("success"), "timeout_seconds": 5}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let negotiation_calls = Arc::new(AtomicUsize::new(0));
    let negotiated_contract = ProviderProtocolContract {
        supports_strict_tool_schema: true,
        supports_parallel_tool_calls: true,
        ..ProviderProtocolContract::default()
    };
    let metadata = negotiated_capability_metadata();
    let agent_loop = AgentLoop::new(
        NegotiatingProvider {
            responses: vec![
                edit_response,
                verify_response,
                ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done"),
            ],
            seen_requests: Arc::clone(&seen_requests),
            negotiation_calls: Arc::clone(&negotiation_calls),
            static_capabilities: ProviderProtocolContract {
                supports_strict_tool_schema: false,
                supports_parallel_tool_calls: false,
                ..ProviderProtocolContract::default()
            },
            negotiated_capabilities: Ok(ProviderProtocolNegotiation {
                contract: negotiated_contract.clone(),
                metadata,
            }),
        },
        workspace_tool_broker_for_test(),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path()).with_sandbox_backend(AgentStrictBackend),
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit").with_max_turns(3);
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(negotiation_calls.load(Ordering::SeqCst), 1);
    let pending = blocked.pending_tool_calls[0].clone();
    let mut checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    checkpoint["provider_protocol_contract"] = serde_json::json!({
        "supports_strict_tool_schema": false,
        "supports_parallel_tool_calls": false
    });
    checkpoint["provider_capability_metadata"] = serde_json::json!({
        "profile": "declared",
        "cache_hit": true
    });

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.request_id.clone(),
        pending.tool_name.clone(),
        pending.resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_tool_call(&resumed_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Completed);
    assert_eq!(negotiation_calls.load(Ordering::SeqCst), 2);
    let requests = seen_requests.lock().expect("seen requests lock");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tool_choice.max_tool_calls, 8);
    assert!(!requests[1].tool_choice.strict_tool_schema);
    assert_eq!(
        resumed.provider_protocol_contract,
        Some(negotiated_contract)
    );
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
}

#[test]
fn agent_loop_preserves_typed_provider_failure_category() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "provider failure");
    let error = ModelError::new(
        ModelErrorKind::AuthError,
        "provider failure text must not drive evaluation classification",
    );
    let result =
        agent_loop_with_response(failed_model_response(error), allow_read_policy()).run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error_category,
        Some(ModelErrorCategory::Authentication)
    );
    assert!(result.provider_protocol_contract.is_some());
    assert!(result.provider_capability_metadata.is_some());
    let status = result.to_run_status();
    assert_eq!(
        status.error_category,
        Some(ModelErrorCategory::Authentication)
    );
}

#[test]
fn agent_loop_preserves_safe_provider_diagnostic() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "provider failure");
    let mut error = ModelError::new(ModelErrorKind::JsonSchemaViolation, "unsafe raw response");
    error.code = Some("provider_response_invalid".to_string());
    error.stage = Some(singularity_model::ProviderErrorStage::ResponseValidation);
    error.validation_errors = vec!["missing_tool_call_id".to_string()];
    let result =
        agent_loop_with_response(failed_model_response(error), allow_read_policy()).run(&input);

    let diagnostic = result
        .provider_diagnostic
        .clone()
        .expect("provider diagnostic");
    assert_eq!(
        diagnostic.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(diagnostic.validation_errors, ["missing_tool_call_id"]);
    assert_eq!(result.to_run_status().provider_diagnostic, Some(diagnostic));
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

fn test_command_script(argument: &str) -> String {
    format!("test-program {argument}")
}

fn plan_tool_call(id: &str, steps: serde_json::Value) -> ModelToolCall {
    tool_call(id, "update_plan", serde_json::json!({"steps": steps}))
}

#[test]
fn agent_loop_read_only_final_answer_completes_without_verification() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run(&input);
    let run_status = result.to_run_status();

    assert_eq!(result.status, AgentStatus::Completed);
    assert!(result.completed);
    assert_eq!(result.model_turns, 1);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert!(result.error.is_none());
    assert_eq!(run_status.status, AgentStatus::Completed);
    assert_eq!(run_status.final_answer.as_deref(), Some("done"));
    assert_eq!(run_status.model_turns, 1);
    assert_eq!(run_status.tool_calls, 0);
    assert!(!run_status.verification.required);
    assert!(!run_status.verification.passed);
    assert_eq!(
        seen_requests.lock().expect("seen requests")[0]
            .tool_choice
            .mode,
        ToolChoiceMode::Auto
    );
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "change the file").with_max_turns(2);
    let mut edit = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    edit.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
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
fn agent_loop_recovers_from_nonportable_unknown_native_tool_without_execution() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "ready").expect("write fixture");
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(3);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "builtin/missing",
        serde_json::json!({}),
    ));
    let mut repaired_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_response.tool_calls.push(tool_call(
        "call_2",
        "read",
        serde_json::json!({
            "path": "README.md",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "recovered");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![response, repaired_response, final_response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.final_answer.as_deref(), Some("recovered"));
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(ToolFailureKind::Visibility)
    );
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_not_visible")
    );
    assert_eq!(result.tool_results[0].tool_name, "builtin/missing");
    let audit = result.tool_results[0]
        .audit_metadata()
        .expect("unknown tool audit");
    assert_eq!(audit["policy_evaluated"], false);
    assert_eq!(audit["executor_started"], false);
    assert!(result.tool_results[1].ok);
    assert_eq!(result.tool_results[1].tool_name, "read");
    assert!(result.verification.unresolved_failures.is_empty());
    assert_eq!(result.recovery_metrics.invalid_tool_call_count, 1);
    assert!(result.error.is_none());
    assert!(result.provider_diagnostic.is_none());
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    let rejected_assistant = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == ModelRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_call_id == "call_1")
        })
        .expect("rejected assistant history");
    let rejected_call = rejected_assistant
        .tool_calls
        .iter()
        .find(|call| call.tool_call_id == "call_1")
        .expect("rejected assistant tool call");
    assert_eq!(rejected_call.tool_name, "tool_rejected");
    assert_eq!(rejected_call.arguments, serde_json::json!({}));
    assert_eq!(rejected_call.raw_arguments, "{}");
    assert!(
        requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.tool_calls)
            .all(|call| call.tool_name.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            ))
    );
    let tool_message = requests[1].messages.last().expect("tool error message");
    assert_eq!(tool_message.role, ModelRole::Tool);
    assert_eq!(tool_message.tool_call_id.as_deref(), Some("call_1"));
    let payload: serde_json::Value =
        serde_json::from_str(&tool_message.content).expect("tool result payload");
    assert_eq!(payload["tool_name"], "tool_rejected");
    assert_eq!(payload["error_code"], "tool_not_visible");
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("builtin/missing"))
    );
}

#[test]
fn direct_tool_mode_rejects_hidden_router_before_policy_or_execution() {
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "invoke_1",
        "invoke_tool",
        serde_json::json!({"path": "README.md"}),
    ));

    let result = agent_loop_with_capabilities(
        vec![response],
        PolicyEngine::new(PermissionProfile::workspace_write()),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract {
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read").with_max_turns(1));

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(ToolFailureKind::Visibility)
    );
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_not_visible")
    );
    assert!(result.approval_requests.is_empty());
    assert!(result.pending_tool_calls.is_empty());
    let audit = result.to_run_status().audit_events[0].clone();
    assert_eq!(audit["policy_evaluated"], false);
    assert_eq!(audit["executor_started"], false);
}

#[test]
fn agent_loop_ask_decision_blocks_without_executing_tool() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({
            "path": "README.md",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));

    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
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
fn agent_loop_executes_admitted_read_batch_in_response_order() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "first file").expect("write first file");
    std::fs::write(dir.path().join("CHANGELOG.md"), "second file").expect("write second file");
    std::fs::write(dir.path().join("Cargo.toml"), "third file").expect("write third file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "read three files").with_max_turns(2);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({
            "path": "README.md",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));
    response.tool_calls.push(tool_call(
        "call_2",
        "read",
        serde_json::json!({
            "path": "CHANGELOG.md",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));
    response.tool_calls.push(tool_call(
        "call_3",
        "read",
        serde_json::json!({
            "path": "Cargo.toml",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_capabilities(
        vec![
            response,
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
        ],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            supports_strict_tool_schema: true,
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 2);
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(result.tool_results[0].tool_call_id, "call_1");
    assert_eq!(result.tool_results[1].tool_call_id, "call_2");
    assert_eq!(result.tool_results[2].tool_call_id, "call_3");
    assert!(
        result.tool_results[0]
            .to_message_payload()
            .to_string()
            .contains("first file")
    );
    assert!(
        result.tool_results[1]
            .to_message_payload()
            .to_string()
            .contains("second file")
    );
    assert!(
        result.tool_results[2]
            .to_message_payload()
            .to_string()
            .contains("third file")
    );
    let requests = seen_requests.lock().expect("seen requests lock");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tool_choice.max_tool_calls, 8);
    assert!(!requests[0].tool_choice.strict_tool_schema);
    assert!(
        requests[0].messages[0]
            .content
            .contains("independent read-only")
    );
}

#[test]
fn agent_loop_rejects_an_invalid_read_batch_before_policy_or_execution() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "must not be returned").expect("write readme");
    std::fs::write(dir.path().join("CHANGELOG.md"), "allowed recovery").expect("write changelog");
    std::fs::write(dir.path().join(".env"), "secret=value").expect("write protected file");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    response.tool_calls.push(tool_call(
        "call_2",
        "read",
        serde_json::json!({"path": ".env", "unexpected": true}),
    ));
    let mut recovery = ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    recovery.tool_calls.push(tool_call(
        "call_3",
        "read",
        serde_json::json!({"path": "CHANGELOG.md"}),
    ));

    let ask_readme = PermissionRule::new(
        "ask_readme",
        SettingsScope::Project,
        PermissionDecisionOutcome::Ask,
    )
    .for_operation(PermissionOperation::Read)
    .for_resource(workspace_resource("README.md"));
    let result = agent_loop_with_capabilities(
        vec![
            response,
            recovery,
            ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done"),
        ],
        allow_read_policy().with_rule(ask_readme),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read files").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Completed);
    assert!(result.approval_requests.is_empty());
    assert!(result.pending_tool_calls.is_empty());
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_batch_rejected")
    );
    assert_eq!(
        result.tool_results[1].failure_kind,
        Some(singularity_tools::ToolFailureKind::Input)
    );
    assert!(
        !result.tool_results[0]
            .to_message_payload()
            .to_string()
            .contains("must not be returned")
    );
    assert!(!result.tool_results.iter().any(|result| {
        result
            .to_message_payload()
            .to_string()
            .contains("secret=value")
    }));
}

#[test]
fn agent_loop_rejects_a_mutating_batch_without_partial_write() {
    let dir = tempfile::tempdir().expect("temp dir");
    let readme = dir.path().join("README.md");
    std::fs::write(&readme, "before").expect("write readme");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    response.tool_calls.push(tool_call(
        "call_2",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let mut recovery = ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    recovery.tool_calls.push(tool_call(
        "call_3",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));

    let result = agent_loop_with_capabilities(
        vec![
            response,
            recovery,
            ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done"),
        ],
        allow_read_write_policy(),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "change file").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        std::fs::read_to_string(readme).expect("read readme"),
        "before"
    );
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("exclusive_tool_requires_single_call")
    );
    assert_eq!(
        result.tool_results[1].error_code.as_deref(),
        Some("tool_batch_rejected")
    );
}

#[test]
fn agent_loop_does_not_create_partial_approval_for_a_batch() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "first").expect("write first file");
    std::fs::write(dir.path().join("CHANGELOG.md"), "second").expect("write second file");
    let ask_readme = PermissionRule::new(
        "ask_readme",
        SettingsScope::Project,
        PermissionDecisionOutcome::Ask,
    )
    .for_operation(PermissionOperation::Read)
    .for_resource(workspace_resource("README.md"));
    let policy = allow_read_policy().with_rule(ask_readme);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    response.tool_calls.push(tool_call(
        "call_2",
        "read",
        serde_json::json!({"path": "CHANGELOG.md"}),
    ));
    let mut recovery = ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    recovery.tool_calls.push(tool_call(
        "call_3",
        "read",
        serde_json::json!({"path": "CHANGELOG.md"}),
    ));

    let result = agent_loop_with_capabilities(
        vec![
            response,
            recovery,
            ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done"),
        ],
        policy,
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read files").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Completed);
    assert!(result.approval_requests.is_empty());
    assert!(result.pending_tool_calls.is_empty());
    assert!(result.approval_checkpoints.is_empty());
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        result.tool_results[1].error_code.as_deref(),
        Some("tool_batch_rejected")
    );
}

#[test]
fn agent_loop_fails_closed_on_mismatched_assistant_tool_calls() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "read a file");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    response
        .assistant_message
        .as_mut()
        .expect("assistant message")
        .tool_calls
        .push(tool_call(
            "call_2",
            "read",
            serde_json::json!({"path": "CHANGELOG.md"}),
        ));

    let result = agent_loop_with_response(response, allow_read_policy()).run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.model_turns, 1);
    assert_eq!(
        result.error.as_deref(),
        Some("model response validation failed: assistant_tool_calls_mismatch")
    );
    assert_eq!(result.error_category, Some(ModelErrorCategory::JsonSchema));
    assert_eq!(
        result
            .provider_diagnostic
            .as_ref()
            .and_then(|diagnostic| diagnostic.stage.clone()),
        Some(singularity_model::ProviderErrorStage::ResponseValidation)
    );
    assert!(result.tool_results.is_empty());
}

#[test]
fn agent_loop_checkpoint_is_bound_and_not_serialized_as_public_result() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit the file");
    let mut response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before approval");
    response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));

    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    let pending = &result.pending_tool_calls[0];
    assert_eq!(pending.request_id, "approval_turn_1_call_1");
    let checkpoint = result
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    assert_eq!(checkpoint["checkpoint_version"], 1);
    assert_eq!(checkpoint["thread_id"], "thread_1");
    assert_eq!(checkpoint["turn_id"], "turn_1");
    assert_eq!(checkpoint["request_id"], "approval_turn_1_call_1");
    assert_eq!(checkpoint["tool_call_id"], "call_1");
    assert_eq!(checkpoint["approval_count"], 1);
    assert_eq!(checkpoint["model_turns"], 1);
    assert_eq!(checkpoint["used_approval_grants"], serde_json::json!([]));
    assert_eq!(checkpoint["tool_results"], serde_json::json!([]));
    let messages = checkpoint["messages"]
        .as_array()
        .expect("checkpoint messages");
    let assistant = messages.last().expect("assistant checkpoint message");
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "before approval");
    assert_eq!(assistant["tool_calls"][0]["tool_call_id"], "call_1");

    let public_result = serde_json::to_string(&result).expect("serialize public result");
    assert!(!public_result.contains("checkpoint_version"));
    assert!(!public_result.contains("raw_arguments"));
    assert!(!public_result.contains("before approval"));
}

#[test]
fn agent_loop_resume_preserves_max_turn_accounting_after_pending_tool_execution() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit the file").with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let agent_loop = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()));
    let blocked = agent_loop.run(&input);
    let pending = blocked.pending_tool_calls[0].clone();
    let checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    let resume_input = input.clone().with_approval_grant(ApprovalGrant::allow(
        pending.request_id.clone(),
        pending.tool_name.clone(),
        pending.resources.clone(),
    ));

    let resumed = agent_loop.resume_pending_tool_call(&resume_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Failed);
    assert_eq!(resumed.model_turns, 1);
    assert_eq!(resumed.approval_count, 1);
    assert_eq!(resumed.tool_results.len(), 1);
    assert_eq!(resumed.error.as_deref(), Some("max turns exceeded"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
}

#[test]
fn agent_loop_resume_rejects_reused_tool_call_id_after_consuming_grant() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "one").expect("write file");
    let first_grant = ApprovalGrant::allow(
        "approval_turn_1_call_1",
        tool_id("edit"),
        [workspace_resource("README.md")],
    );
    let second_grant = ApprovalGrant::allow(
        "approval_turn_1_call_2",
        tool_id("edit"),
        [workspace_resource("README.md")],
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit twice")
        .with_max_turns(4)
        .with_approval_grant(first_grant.clone());
    let mut first_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first_response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "one",
            "replacement": "two"
        }),
    ));
    let mut second_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    second_response.tool_calls.push(tool_call(
        "call_2",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "two",
            "replacement": "three"
        }),
    ));
    let mut reused_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    reused_response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "three",
            "replacement": "four"
        }),
    ));
    let agent_loop = agent_loop_with_responses_and_requests(
        vec![first_response, second_response, reused_response],
        PolicyEngine::new(PermissionProfile::workspace_write()),
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()));

    let blocked = agent_loop.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    let pending = blocked.pending_tool_calls[0].clone();
    let checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    assert_eq!(
        checkpoint["used_approval_grants"],
        serde_json::json!(["approval_turn_1_call_1"])
    );
    let resume_input = input
        .with_approval_grant(second_grant)
        .with_approval_grant(first_grant);

    let resumed = agent_loop.resume_pending_tool_call(&resume_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Failed);
    assert_eq!(resumed.model_turns, 3);
    assert_eq!(resumed.approval_count, 1);
    assert!(resumed.approval_requests.is_empty());
    assert_eq!(
        resumed.error.as_deref(),
        Some("tool execution failed: tool_denied")
    );
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "three"
    );
}

#[test]
fn agent_loop_resume_rejects_tampered_completion_checkpoint() {
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit the file");
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let agent_loop = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    );
    let blocked = agent_loop.run(&input);
    let pending = blocked.pending_tool_calls[0].clone();
    let mut checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    checkpoint["completion"]["workspace_mutated"] = serde_json::json!(true);
    let resume_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.request_id.clone(),
        pending.tool_name.clone(),
        pending.resources.clone(),
    ));

    let resumed = agent_loop.resume_pending_tool_call(&resume_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Failed);
    assert_eq!(
        resumed.error.as_deref(),
        Some("approval checkpoint completion state mismatch")
    );
    assert!(resumed.tool_results.is_empty());
}

#[test]
fn agent_loop_sends_project_instructions_as_developer_message_without_serializing_them() {
    let project_instructions = "root instructions\n\nchild instructions";
    let input = AgentLoopInput::new("thread_1", "turn_1", "user goal")
        .with_project_instructions(project_instruction_snapshot(project_instructions));
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
    let developer = &requests[0].messages[0].content;
    assert!(developer.contains("You are a coding agent working in the current workspace."));
    assert!(
        developer.contains(
            "Issue at most one tool call per assistant response and wait for its result."
        )
    );
    assert!(developer.contains(
        "For multi-step work, keep a concise update_plan plan; revise it when evidence or failure changes the approach, and complete it before the final answer. Skip plans for simple read-only or single-step work."
    ));
    assert!(developer.ends_with(project_instructions));
    assert_eq!(requests[0].messages[1].content, "user goal");
    assert!(
        !requests[0].messages[1]
            .content
            .contains(project_instructions)
    );
    assert!(!requests[0].tools.iter().any(|tool| {
        serde_json::to_string(tool)
            .expect("serialize tool")
            .contains(project_instructions)
    }));
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
        .with_project_instructions(project_instruction_snapshot(project_instructions))
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
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["previous user", "previous assistant", "current user",]
    );
    assert!(request_messages[0].content.ends_with(project_instructions));
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
    assert!(requests[0].tools.iter().any(|tool| tool.name == "read"));
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "invoke_tool")
    );
    assert_eq!(
        requests[0].model_preferences.model_name.as_deref(),
        Some("gpt-test")
    );
    let context_trace = result.context_trace.as_ref().expect("context trace");
    assert_eq!(context_trace.included_item_ids, ["input_1"]);
    assert_eq!(
        context_trace.budget["model_context_window"],
        serde_json::json!(DEFAULT_MAX_CONTEXT_TOKENS)
    );
}

#[test]
fn direct_tool_mode_rejects_capacity_shortfall_without_implicit_routing() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_capabilities(
        vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "unused",
        )],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            max_tools_per_request: 2,
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("provider direct tool-definition limit (2) is below the required tool count (4)")
    );
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_uses_provider_capabilities_for_budget_metadata() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let capabilities = ProviderProtocolContract {
        max_context_tokens: 64_000,
        max_output_tokens: 128,
        ..ProviderProtocolContract::default()
    };
    let result = agent_loop_with_capabilities(
        vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        capabilities,
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "hello"));

    assert_eq!(result.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    let budget = &result.context_trace.as_ref().expect("context trace").budget;
    assert_eq!(budget["model_context_window"], 64_000);
    assert_eq!(budget["reserved_output_tokens"], 128);
    assert_eq!(requests[0].model_preferences.max_output_tokens, Some(128));
    assert!(
        budget["reserved_request_tokens"].as_u64().unwrap()
            >= budget["reserved_output_tokens"].as_u64().unwrap()
    );
    assert!(budget["input_token_budget"].as_u64().unwrap() < u64::from(DEFAULT_MAX_CONTEXT_TOKENS));
}

#[test]
fn agent_loop_rejects_requested_output_above_provider_capability() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = agent_loop_with_capabilities(
        vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "must not be used",
        )],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            max_output_tokens: 128,
            ..ProviderProtocolContract::default()
        },
    );
    let mut input = AgentLoopInput::new("thread_1", "turn_1", "hello");
    input.model_preferences.max_output_tokens = Some(129);

    let result = agent_loop.run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("requested output tokens (129) exceed provider output limit (128)")
    );
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_rejects_unsupported_tool_capability_before_provider() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let result = agent_loop_with_capabilities(
        vec![response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            supports_tools: false,
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "hello"));

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.model_turns, 0);
    assert_eq!(
        result.error.as_deref(),
        Some("model request validation failed: provider_does_not_support_tools")
    );
    assert!(result.tool_results.is_empty());
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_rejects_zero_tool_definition_capacity_before_provider() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_capabilities(
        vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "must not be used",
        )],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            max_tools_per_request: 0,
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "hello"));

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.model_turns, 0);
    assert_eq!(
        result.error.as_deref(),
        Some("provider direct tool-definition limit (0) is below the required tool count (4)")
    );
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_executes_workspace_read_tool_with_safe_tool_result() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "hello from workspace").expect("write readme");
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "read the file").with_max_turns(2);
    let mut oversized_call = tool_call(
        "call_1",
        "read",
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
fn agent_loop_compacts_large_tool_output_before_the_next_model_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bound_cwd = ".";
    let required_argv = test_command("second-success");
    let required_digest = command_script_scope_digest_with_policy(
        &required_argv.join(" "),
        bound_cwd,
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut required_verification =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    required_verification.tool_calls.push(tool_call(
        "call_2",
        "command",
        serde_json::json!({
            "command": required_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let capabilities = ProviderProtocolContract {
        max_context_tokens: 1_400,
        max_output_tokens: 128,
        ..ProviderProtocolContract::default()
    };

    let result = agent_loop_with_capabilities(
        vec![command_response, required_verification, final_response],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        capabilities,
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(LargeOutputBackend))
    .run(
        &AgentLoopInput::new("thread_1", "turn_1", "run the command")
            .with_max_turns(3)
            .with_verification_requirements([AgentVerificationRequirement::new(
                required_digest,
                1,
            )]),
    );

    assert_eq!(result.status, AgentStatus::Completed);
    let context_trace = result.context_trace.as_ref().expect("context trace");
    assert_eq!(context_trace.compaction_count, 2);
    assert!(context_trace.compacted_message_count >= 1);
    assert!(
        context_trace.last_compaction_before_tokens > context_trace.last_compaction_after_tokens
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == ModelRole::Developer
            && message.content.contains("agent_context_compaction")
            && message
                .content
                .contains("Run every exact verification command")
    }));
    assert!(requests[1].messages.iter().any(|message| {
        message.role == ModelRole::Tool && message.content.contains("\"compacted\":true")
    }));
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| message.content.contains("large-safe-output"))
    );
}

#[test]
fn exact_verification_ignores_wrong_or_pre_mutation_results_and_counts_duplicates() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let bound_cwd = ".";
    let required_digest = command_script_scope_digest_with_policy(
        &test_command_script("success"),
        bound_cwd,
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit and verify")
        .with_max_turns(7)
        .with_verification_requirements([AgentVerificationRequirement::new(required_digest, 2)]);

    let command_response = |turn_index: u32, call_id: &str, argument: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_1_{turn_index}"),
            format!("response_{turn_index}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "command",
            serde_json::json!({
                "command": test_command_script(argument),
                "cwd": ".",
                "timeout_seconds": 5
            }),
        ));
        response
    };
    let pre_mutation_verification = command_response(0, "command_0", "success");
    let mut edit = ModelTurnResponse::completed("model_request_turn_1_1", "response_1", "");
    edit.tool_calls.push(tool_call(
        "edit_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let wrong_verification = command_response(2, "command_2", "second-success");
    let first_required_verification = command_response(3, "command_3", "success");
    let premature_final =
        ModelTurnResponse::completed("model_request_turn_1_4", "response_4", "not done");
    let second_required_verification = command_response(5, "command_5", "success");
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_6", "response_6", "done");
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_responses_and_requests(
        vec![
            pre_mutation_verification,
            edit,
            wrong_verification,
            first_required_verification,
            premature_final,
            second_required_verification,
            final_response,
        ],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert!(result.verification.required);
    assert!(result.verification.passed);
    assert_eq!(result.verification.required_command_count, 2);
    assert_eq!(result.verification.satisfied_command_count, 2);
    assert_eq!(result.verification.successful_command_count, 4);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("README.md")).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 7);
    assert!(
        requests[..6]
            .iter()
            .all(|request| request.tool_choice.mode == ToolChoiceMode::Auto)
    );
    assert_eq!(requests[6].tool_choice.mode, ToolChoiceMode::None);
    assert_eq!(requests[6].tool_choice.max_tool_calls, 0);
    assert!(requests[6].tools.is_empty());
}

#[test]
fn policy_denial_is_a_recoverable_non_execution_result() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.md"), "unchanged").expect("fixture");
    std::fs::write(workspace.path().join("CHANGELOG.md"), "allowed").expect("fixture");
    let mut denied = ModelTurnResponse::completed("model_request_turn_1_0", "response_0", "");
    denied.tool_calls.push(tool_call(
        "denied",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let mut allowed = ModelTurnResponse::completed("model_request_turn_1_1", "response_1", "");
    allowed.tool_calls.push(tool_call(
        "allowed",
        "read",
        serde_json::json!({"path": "CHANGELOG.md"}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_policy().with_rule(
        PermissionRule::new(
            "deny_readme",
            SettingsScope::Project,
            PermissionDecisionOutcome::Deny,
        )
        .for_operation(PermissionOperation::Read)
        .for_resource(workspace_resource("README.md")),
    );

    let result = agent_loop_with_responses_and_requests(
        vec![denied, allowed, final_response],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(workspace.path()))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read if allowed").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_denied")
    );
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(singularity_tools::ToolFailureKind::Policy)
    );
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().any(|message| {
        message.role == ModelRole::Tool && message.content.contains("\"failure_kind\":\"policy\"")
    }));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("README.md")).expect("fixture remains"),
        "unchanged"
    );
}

#[test]
fn approval_resume_preserves_exact_verification_and_compaction_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let bound_cwd = ".";
    let sandbox_mode = SandboxFilesystemMode::WorkspaceWrite;
    let network_access = SandboxNetworkMode::Denied;
    let first_argv = test_command("success");
    let second_argv = test_command("second-success");
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit and verify twice")
        .with_max_turns(3)
        .with_verification_requirements([
            AgentVerificationRequirement::new(
                command_script_scope_digest_with_policy(
                    &first_argv.join(" "),
                    bound_cwd,
                    5,
                    sandbox_mode.clone(),
                    network_access.clone(),
                ),
                1,
            ),
            AgentVerificationRequirement::new(
                command_script_scope_digest_with_policy(
                    &second_argv.join(" "),
                    bound_cwd,
                    5,
                    sandbox_mode.clone(),
                    network_access.clone(),
                ),
                1,
            ),
        ]);

    let mut edit = ModelTurnResponse::completed("model_request_turn_1_0", "response_0", "");
    edit.tool_calls.push(tool_call(
        "edit_0",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let command_response = |turn_index: u32, call_id: &str, argv: &[String]| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_1_{turn_index}"),
            format!("response_{turn_index}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "command",
            serde_json::json!({
                "command": argv.join(" "),
                "cwd": ".",
                "timeout_seconds": 5
            }),
        ));
        response
    };
    let first_verification = command_response(1, "verify_1", &first_argv);
    let pending_verification = command_response(2, "verify_2", &second_argv);
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_3", "done");
    let first_command_resource =
        typed_command_resource(singularity_tools::command_script_scope_digest_with_policy(
            &first_argv.join(" "),
            ".",
            5,
            sandbox_mode.clone(),
            network_access.clone(),
        ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
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
                "allow_first_verification",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Execute)
            .for_resource(first_command_resource),
        );
    let capabilities = ProviderProtocolContract {
        max_context_tokens: 1_400,
        max_output_tokens: 128,
        ..ProviderProtocolContract::default()
    };
    let agent_loop = agent_loop_with_capabilities(
        vec![
            edit,
            first_verification,
            pending_verification,
            final_response,
        ],
        policy,
        Arc::new(Mutex::new(Vec::new())),
        capabilities,
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(LargeOutputBackend));

    let blocked = agent_loop.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(blocked.verification.required_command_count, 2);
    assert_eq!(blocked.verification.satisfied_command_count, 1);
    let pending = blocked.pending_tool_calls[0].clone();
    let checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    assert_eq!(checkpoint["context_trace"]["compaction_count"], 1);
    assert_eq!(
        checkpoint["completion"]["terminal_command_scope_digests"]
            .as_array()
            .expect("terminal command observations")
            .len(),
        1
    );

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.request_id.clone(),
        pending.tool_name.clone(),
        pending.resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_tool_call(&resumed_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Completed);
    assert_eq!(resumed.model_turns, 4);
    assert_eq!(resumed.model_turn_limit, 3);
    assert!(resumed.verification.passed);
    assert_eq!(resumed.verification.required_command_count, 2);
    assert_eq!(resumed.verification.satisfied_command_count, 2);
    assert!(
        resumed
            .context_trace
            .as_ref()
            .is_some_and(|trace| trace.compaction_count >= 1)
    );
}

#[test]
fn agent_loop_approval_grant_allows_workspace_mutation_without_policy_reask() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello")
        .with_approval_grant(ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("edit"),
            [workspace_resource("README.md")],
        ))
        .with_max_turns(3);
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before edit");
    tool_response.tool_calls.push(tool_call(
        "call_1",
        "edit",
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
        "command",
        serde_json::json!({"command": "cargo test", "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![tool_response, verification_response, final_response],
        PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
    assert_eq!(requests[1].messages[2].content, "before edit");
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(4);
    let mut failing_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    failing_tool_response.tool_calls.push(tool_call(
        "call_1",
        "edit",
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
        "edit",
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
        "command",
        serde_json::json!({"command": "cargo test", "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
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
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("expected_content_missing")
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
    assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::Auto);
    let feedback = requests[1].messages.last().expect("tool feedback");
    assert_eq!(feedback.role, ModelRole::Tool);
    let payload: serde_json::Value =
        serde_json::from_str(&feedback.content).expect("structured tool payload");
    assert_eq!(payload["error_code"], "expected_content_missing");
    assert!(
        payload["content"]["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("expected content not found"))
    );
    assert!(payload.get("preview").is_none());
}

#[test]
fn agent_loop_returns_invalid_command_arguments_to_model_for_repair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput::new("thread_1", "turn_1", "run verification").with_max_turns(4);
    let mut malformed_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    malformed_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({"command": 17, "timeout_seconds": 5}),
    ));
    let mut repaired_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_response.tool_calls.push(tool_call(
        "call_2",
        "command",
        serde_json::json!({"command": test_command_script("success"), "timeout_seconds": 5}),
    ));
    let mut second_repaired_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    second_repaired_response.tool_calls.push(tool_call(
        "call_3",
        "command",
        serde_json::json!({"command": test_command_script("second-success"), "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_execute",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_responses_and_requests(
        vec![
            malformed_response,
            repaired_response,
            second_repaired_response,
            final_response,
        ],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 4);
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    assert!(result.tool_results[1].ok);
    assert!(result.tool_results[2].ok);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    let run_status = result.to_run_status();
    assert_eq!(run_status.audit_events.len(), 3);
    assert_eq!(run_status.audit_events[0]["argument_validation"], "failed");
    assert_eq!(
        run_status.audit_events[0]["argument_validation_code"],
        "command_not_string"
    );
    assert_eq!(run_status.audit_events[0]["policy_evaluated"], false);
    assert_eq!(run_status.audit_events[0]["executor_started"], false);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[2].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[3].tool_choice.mode, ToolChoiceMode::Auto);
    let feedback = requests[1].messages.last().expect("tool feedback");
    assert_eq!(feedback.role, ModelRole::Tool);
    let payload: serde_json::Value =
        serde_json::from_str(&feedback.content).expect("structured tool payload");
    assert_eq!(payload["error_code"], "invalid_tool_arguments");
    assert_eq!(payload["content"]["validation_code"], "command_not_string");
    assert_eq!(
        payload["content"]["retry_inputs"].as_array().map(Vec::len),
        Some(0)
    );
    assert!(
        payload["content"]["summary"]
            .as_str()
            .is_some_and(|summary| { summary.contains("exact JSON input allowed by this schema") })
    );
    assert!(payload.get("preview").is_none());
    assert!(!feedback.content.contains("raw_arguments"));
    assert!(
        run_status.audit_events[0]
            .get("approval_decision")
            .is_none()
    );
    assert_eq!(
        run_status.audit_events[0]["sandbox_backend"],
        "not_executed"
    );
    assert_eq!(
        run_status.audit_events[1]["approval_decision"],
        "allowed_by_policy"
    );
}

#[test]
fn agent_loop_validates_patch_arguments_before_policy() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "content").expect("write file");
    let input =
        AgentLoopInput::new("thread_1", "turn_1", "inspect the workspace").with_max_turns(1);
    let mut malformed_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    malformed_response.tool_calls.push(tool_call(
        "call_1",
        "patch",
        serde_json::json!({"changes": "README.md"}),
    ));
    let result = agent_loop_with_response(
        malformed_response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    let run_status = result.to_run_status();
    assert_eq!(run_status.audit_events.len(), 1);
    assert_eq!(run_status.audit_events[0]["argument_validation"], "failed");
    assert_eq!(run_status.audit_events[0]["policy_evaluated"], false);
    assert_eq!(run_status.audit_events[0]["executor_started"], false);
    assert!(
        run_status.audit_events[0]
            .get("approval_decision")
            .is_none()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "content"
    );
}

#[test]
fn agent_loop_command_fails_closed_without_sandbox_backend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command").with_max_turns(2);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(singularity_tools::ToolFailureKind::Sandbox)
    );
    let audit = result.tool_results[0]
        .audit_metadata()
        .expect("command audit metadata");
    assert_eq!(
        audit["command_scope_digest"],
        command_script_scope_digest_with_policy(
            &test_command_script("success"),
            ".",
            5,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command").with_max_turns(2);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command").with_max_turns(3);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("failure"),
            "timeout_seconds": 5
        }),
    ));
    let mut repaired_command_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_command_response.tool_calls.push(tool_call(
        "call_2",
        "command",
        serde_json::json!({
            "command": test_command_script("repaired"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "handled failure");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(singularity_tools::ToolFailureKind::Execution)
    );
    assert!(result.tool_results[1].ok);
    assert!(result.verification.unresolved_failures.is_empty());
    assert_eq!(result.final_answer.as_deref(), Some("handled failure"));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    let tool_message = requests[1].messages.last().expect("tool result message");
    assert_eq!(tool_message.role, ModelRole::Tool);
    assert_eq!(tool_message.tool_call_id.as_deref(), Some("call_1"));
    let payload: serde_json::Value =
        serde_json::from_str(&tool_message.content).expect("tool result payload");
    assert_eq!(payload["error_code"], "command_exit_nonzero");
    assert_eq!(payload["content"]["execution_status"], "completed");
    assert_eq!(payload["content"]["stderr_preview"], "failed");
}

#[test]
fn agent_loop_returns_unavailable_executable_to_model_for_repair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command").with_max_turns(3);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": "missing-host-tool",
            "timeout_seconds": 5
        }),
    ));
    let mut repaired_command_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired_command_response.tool_calls.push(tool_call(
        "call_2",
        "command",
        serde_json::json!({
            "command": test_command_script("repaired"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "recovered");
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
        AgentExecutableUnavailableBackend {
            calls: AtomicUsize::new(0),
        },
    ))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("command_executable_unavailable")
    );
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(ToolFailureKind::Capability)
    );
    assert!(result.tool_results[1].ok);
    assert_eq!(result.final_answer.as_deref(), Some("recovered"));
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
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({"command": "cargo test", "timeout_seconds": 30}),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
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
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(singularity_tools::ToolFailureKind::Cancelled)
    );
}

#[test]
fn agent_loop_rejects_model_selected_network_before_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let argv = test_command("must-not-execute");
    let resource =
        typed_command_resource(singularity_tools::command_script_scope_digest_with_policy(
            &argv.join(" "),
            ".",
            5,
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Allowed,
        ));
    let input = AgentLoopInput::new("thread_1", "turn_1", "run network command")
        .with_approval_grant(ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("command"),
            [resource.clone()],
        ))
        .with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": argv.join(" "),
            "network_access": "allowed",
            "timeout_seconds": 5
        }),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
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
        Some("invalid_tool_arguments")
    );
    assert_eq!(result.approval_count, 0);
    assert_eq!(
        result.to_run_status().audit_events[0]["policy_evaluated"],
        false
    );
    assert_eq!(
        result.to_run_status().audit_events[0]["executor_started"],
        false
    );
}

#[test]
fn agent_loop_uses_exact_command_binding_without_exposing_execution_policy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let model_input = serde_json::json!({
        "command": test_command_script("success"),
        "cwd": ".",
        "timeout_seconds": 5,
    });
    let execution_input = model_input.clone();
    let mut registry = ToolRegistry::default();
    let mut command = workspace_tool_entries()
        .into_iter()
        .find(|entry| entry.spec.name == "command")
        .expect("command entry");
    command
        .spec
        .restrict_to_input_bindings(vec![(model_input.clone(), execution_input)])
        .expect("exact command binding");
    registry.register(command).expect("register command");

    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response
        .tool_calls
        .push(tool_call("call_1", "command", model_input));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_execute",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![command_response, final_response],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        ToolBroker::new(registry),
        policy,
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend));

    let result = agent_loop
        .run(&AgentLoopInput::new("thread_1", "turn_1", "run the exact command").with_max_turns(2));

    assert_eq!(result.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    let command_schema = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "command")
        .expect("projected command schema")
        .parameters_schema
        .to_string();
    assert!(!command_schema.contains("sandbox_mode"));
    assert!(!command_schema.contains("network_access"));
    let run_status = result.to_run_status();
    let audit = &run_status.audit_events[0];
    assert_eq!(audit["sandbox_mode"], "workspace_write");
    assert_eq!(audit["network_access"], "denied");
    assert_eq!(audit["local_process_fallback"], false);
}

#[test]
fn agent_loop_command_approval_binds_exact_resource_and_rejects_tampered_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let command_script = test_command_script("success");
    let command_resource =
        typed_command_resource(singularity_tools::command_script_scope_digest_with_policy(
            &command_script,
            ".",
            5,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));
    let mismatched_resource =
        typed_command_resource(singularity_tools::command_script_scope_digest_with_policy(
            &command_script,
            ".",
            6,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        ));
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command")
        .with_approval_grant(ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("command"),
            [mismatched_resource],
        ))
        .with_max_turns(2);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    let model_input = serde_json::json!({
        "command": command_script,
        "cwd": ".",
        "timeout_seconds": 5,
    });
    let execution_input = model_input.clone();
    command_response
        .tool_calls
        .push(tool_call("call_1", "command", model_input.clone()));
    let mut registry = ToolRegistry::default();
    let mut command = workspace_tool_entries()
        .into_iter()
        .find(|entry| entry.spec.name == "command")
        .expect("command entry");
    command
        .spec
        .restrict_to_input_bindings(vec![(model_input, execution_input)])
        .expect("exact command binding");
    registry.register(command).expect("register command");
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                command_response,
                ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        ToolBroker::new(registry),
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).with_sandbox_backend(AgentStrictBackend));

    let result = agent_loop.run(&input);

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
    let pending = result
        .pending_tool_calls
        .first()
        .expect("pending command")
        .clone();
    let pending_arguments: serde_json::Value =
        serde_json::from_str(&pending.raw_arguments).expect("pending arguments");
    assert_eq!(pending_arguments["cwd"], ".");
    assert_eq!(pending_arguments["timeout_seconds"], 5);
    assert_eq!(pending_arguments["command"], command_script);

    let mut tampered = pending.clone();
    let mut tampered_arguments = pending_arguments;
    tampered_arguments["command"] = serde_json::json!("different command");
    tampered.raw_arguments = tampered_arguments.to_string();
    let mut checkpoint = result
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    checkpoint["raw_arguments"] = serde_json::json!(tampered.raw_arguments.clone());
    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.request_id,
        pending.tool_name,
        pending.resources,
    ));
    let resumed = agent_loop.resume_pending_tool_call(&resumed_input, &tampered, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Failed);
    assert_eq!(
        resumed.error.as_deref(),
        Some("invalid pending execution input: input_not_allowed")
    );
    assert!(resumed.tool_results.is_empty());
}

#[test]
fn agent_loop_command_audit_records_sandbox_approval_and_provenance() {
    let dir = tempfile::tempdir().expect("temp dir");
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command").with_max_turns(2);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let mut profile = PermissionProfile::workspace_write();
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
    let run_status = result.to_run_status();

    assert_eq!(run_status.status, AgentStatus::Completed);
    assert_eq!(run_status.audit_events.len(), 1);
    assert_eq!(
        run_status.audit_events[0]["sandbox_mode"],
        "workspace_write"
    );
    assert!(run_status.audit_events[0].get("cwd").is_none());
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
        command_script_scope_digest_with_policy(
            &test_command_script("success"),
            ".",
            5,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Allowed,
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

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "agent command ok").with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

struct LargeOutputBackend;

impl SandboxBackend for LargeOutputBackend {
    fn name(&self) -> &'static str {
        "large_output_strict_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "large-safe-output\n".repeat(2_000))
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "large-safe-output\n".repeat(2_000))
            .with_sandbox_execution(
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

    fn execute_script_cancellable(
        &self,
        request: &CommandScriptRequest,
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

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        let result = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
        } else {
            CommandResult::completed(&request.command_id, "repaired")
        };
        result.with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

struct AgentExecutableUnavailableBackend {
    calls: AtomicUsize,
}

impl SandboxBackend for AgentExecutableUnavailableBackend {
    fn name(&self) -> &'static str {
        "agent_executable_unavailable_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        let result = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executable_unavailable(
                &request.command_id,
                "required executable 'missing-host-tool' was not found on host PATH",
            )
        } else {
            CommandResult::completed(&request.command_id, "repaired")
        };
        result.with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        let result = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executable_unavailable(
                &request.command_id,
                "required executable 'missing-host-tool' was not found on host PATH",
            )
        } else {
            CommandResult::completed(&request.command_id, "repaired")
        };
        result.with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

#[test]
fn agent_loop_approval_grant_matches_request_id_and_is_single_use() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "one").expect("write file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_approval_grant(
        ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("edit"),
            [workspace_resource("README.md")],
        ),
    );
    let mut first_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first_tool_response.tool_calls.push(tool_call(
        "call_1",
        "edit",
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
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "two",
            "replacement": "three"
        }),
    ));

    let result = agent_loop_with_responses_and_requests(
        vec![first_tool_response, second_tool_response],
        PolicyEngine::new(PermissionProfile::workspace_write()),
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
        let input = AgentLoopInput::new("thread_1", "turn_1", "hello")
            .with_approval_grant(ApprovalGrant::allow(
                "approval_turn_1_call_1",
                tool_id("edit"),
                [workspace_resource(sensitive_path)],
            ))
            .with_max_turns(1);
        let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
        response.tool_calls.push(tool_call(
            "call_1",
            "edit",
            serde_json::json!({
                "path": sensitive_path,
                "expected": "TOKEN=secret",
                "replacement": "TOKEN=changed"
            }),
        ));

        let result = agent_loop_with_response(
            response,
            PolicyEngine::new(PermissionProfile::workspace_write()),
        )
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

        assert_eq!(result.status, AgentStatus::Failed, "{sensitive_path}");
        assert_eq!(
            result.tool_results[0].error_code.as_deref(),
            Some("protected_path"),
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello")
        .with_approval_grant(ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("patch"),
            [workspace_resource(".env")],
        ))
        .with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "patch",
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
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("protected_path")
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "patch",
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
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
        .with_rule(
            PermissionRule::new(
                "allow_first",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource(workspace_resource("first.md")),
        )
        .with_rule(
            PermissionRule::new(
                "deny_second",
                SettingsScope::Project,
                PermissionDecisionOutcome::Deny,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource(workspace_resource("second.md")),
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "patch",
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
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_first",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write)
        .for_resource(workspace_resource("first.md")),
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello")
        .with_approval_grant(ApprovalGrant::allow(
            "approval_turn_1_call_1",
            tool_id("patch"),
            [workspace_resource("first.md")],
        ))
        .with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "patch",
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
        PolicyEngine::new(PermissionProfile::workspace_write()),
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(1);
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_1",
        "read",
        serde_json::json!({"path": ".env"}),
    ));

    let result = agent_loop_with_response(response, allow_read_policy())
        .with_workspace_tools(WorkspaceTools::new(dir.path()))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("protected_path")
    );
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(singularity_tools::ToolFailureKind::ProtectedPath)
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
    let serialized = serde_json::to_value(&context).expect("serialize context bundle");
    for removed_field in [
        "run_id",
        "task_id",
        "phase_id",
        "model",
        "provider",
        "compression_snapshot_id",
        "retrieval_query",
        "created_at",
        "metadata",
        "bundle_id",
        "render_policy",
        "bundle_digest",
    ] {
        assert!(!serialized.as_object().unwrap().contains_key(removed_field));
    }
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
fn context_assembly_does_not_truncate_the_current_turn() {
    let current = AgentContextItem::user("input_1", "current turn content");
    let max_tokens = current.token_count.saturating_sub(1);

    let context = assemble_context_items(&[current], max_tokens);

    assert!(context.messages.is_empty());
    assert_eq!(context.included_item_ids, Vec::<String>::new());
    assert_eq!(context.excluded_item_ids, vec!["input_1"]);
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
        },
        AgentContextItem {
            item_id: "user_1".to_string(),
            role: "user".to_string(),
            content: "fix tests".to_string(),
            priority: AgentContextItemPriority::CurrentTurn,
            token_count: 6,
            public: true,
            evaluator_only: false,
        },
        AgentContextItem {
            item_id: "eval_1".to_string(),
            role: "system".to_string(),
            content: "hidden scorer".to_string(),
            priority: AgentContextItemPriority::System,
            token_count: 4,
            public: true,
            evaluator_only: true,
        },
        AgentContextItem {
            item_id: "tool_safe".to_string(),
            role: "tool".to_string(),
            content: "safe preview".to_string(),
            priority: AgentContextItemPriority::Evidence,
            token_count: 5,
            public: true,
            evaluator_only: false,
        },
    ];

    let context = assemble_context_items(&items, 11);

    assert_eq!(context.included_item_ids, vec!["user_1", "tool_safe"]);
    assert_eq!(context.excluded_item_ids, vec!["tool_raw", "eval_1"]);
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0]["role"], "user");
    assert_eq!(context.messages[1]["role"], "tool");
    assert_eq!(context.budget["message_tokens"], 11);
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
    assert!(result.provider_protocol_contract.is_some());
    assert!(result.provider_capability_metadata.is_some());
    assert_eq!(result.model_turns, 0);
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_rejects_requested_output_above_provider_limit() {
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
        Some("requested output tokens (128000) exceed provider output limit (4096)")
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

#[test]
fn plan_update_is_brokered_and_returns_safe_summary() {
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    plan_response.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": "inspect the workspace", "status": "completed"}]),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_plan_capabilities(
        vec![plan_response, final_response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.plan_update_count, 1);
    assert_eq!(
        result.plan,
        Some(AgentPlan {
            steps: vec![AgentPlanStep {
                step: "inspect the workspace".to_string(),
                status: AgentPlanStepStatus::Completed,
            }],
        })
    );
    assert_eq!(result.recovery_metrics, AgentRecoveryMetrics::default());
    let payload = result.tool_results[0].to_message_payload();
    assert_eq!(payload["tool_name"], "update_plan");
    assert_eq!(
        payload["content"]["plan"]["steps"][0]["step"],
        "inspect the workspace"
    );
    assert_eq!(
        payload["content"]["plan"]["steps"][0]["status"],
        "completed"
    );
    assert_eq!(
        seen_requests.lock().expect("seen requests")[0].tools.len(),
        5
    );
}

#[test]
fn plan_update_rejects_empty_duplicate_and_multiple_in_progress_steps() {
    for steps in [
        serde_json::json!([]),
        serde_json::json!([
            {"step": "same", "status": "pending"},
            {"step": "same", "status": "completed"}
        ]),
        serde_json::json!([
            {"step": "first", "status": "in_progress"},
            {"step": "second", "status": "in_progress"}
        ]),
    ] {
        let input: AgentPlanUpdateInput =
            serde_json::from_value(serde_json::json!({"steps": steps})).expect("plan input shape");
        assert!(input.into_plan().is_err());
    }

    let unknown_field = serde_json::from_value::<AgentPlanUpdateInput>(serde_json::json!({
        "steps": [{"step": "valid", "status": "pending"}],
        "unexpected": true
    }));
    assert!(unknown_field.is_err());

    let too_many = AgentPlanUpdateInput {
        steps: (0..65)
            .map(|index| AgentPlanStep {
                step: format!("step {index}"),
                status: AgentPlanStepStatus::Pending,
            })
            .collect(),
    };
    assert!(too_many.into_plan().is_err());
    let too_long = AgentPlanUpdateInput {
        steps: vec![AgentPlanStep {
            step: "x".repeat(513),
            status: AgentPlanStepStatus::Pending,
        }],
    };
    assert!(too_long.into_plan().is_err());
}

#[test]
fn plan_tool_contract_preserves_actionable_validation_causes() {
    let spec = agent_control_tool_entries()
        .into_iter()
        .find(|entry| entry.spec.name == "update_plan")
        .expect("plan tool entry")
        .spec;
    let too_many = (0..65)
        .map(|index| serde_json::json!({"step": format!("step {index}"), "status": "pending"}))
        .collect::<Vec<_>>();
    let cases = [
        (serde_json::json!({}), "plan_input_shape_invalid"),
        (serde_json::json!({"steps": []}), "plan_steps_empty"),
        (
            serde_json::json!({"steps": [{"step": " ", "status": "pending"}]}),
            "plan_step_empty",
        ),
        (
            serde_json::json!({"steps": [{"step": "x".repeat(513), "status": "pending"}]}),
            "plan_step_too_long",
        ),
        (
            serde_json::json!({
                "steps": [
                    {"step": "same", "status": "pending"},
                    {"step": "same", "status": "completed"}
                ]
            }),
            "plan_step_duplicate",
        ),
        (
            serde_json::json!({
                "steps": [
                    {"step": "first", "status": "in_progress"},
                    {"step": "second", "status": "in_progress"}
                ]
            }),
            "plan_multiple_in_progress",
        ),
        (
            serde_json::json!({"steps": too_many}),
            "plan_step_limit_exceeded",
        ),
    ];

    for (input, expected_code) in cases {
        let error = spec
            .prepare_model_input(&input)
            .expect_err("invalid plan input must be rejected");
        assert_eq!(error.code, expected_code);
    }
}

#[test]
fn agent_loop_aggregates_provider_attempts_latency_and_token_usage() {
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    plan_response.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": "inspect", "status": "completed"}]),
    ));
    plan_response.usage = ModelUsage {
        input_tokens: 100,
        output_tokens: 20,
        total_tokens: 120,
        cached_input_tokens: 30,
        reasoning_tokens: 5,
        cost_estimate: None,
    };
    plan_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 2,
        retry_count: 1,
        latency_ms: 80,
    });
    let mut final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    final_response.usage = ModelUsage {
        input_tokens: 140,
        output_tokens: 10,
        total_tokens: 150,
        cached_input_tokens: 40,
        reasoning_tokens: 2,
        cost_estimate: None,
    };
    final_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        retry_count: 0,
        latency_ms: 25,
    });

    let result = agent_loop_with_plan_capabilities(
        vec![plan_response, final_response],
        allow_read_policy(),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract::default(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_usage.input_tokens, 240);
    assert_eq!(result.model_usage.output_tokens, 30);
    assert_eq!(result.model_usage.total_tokens, 270);
    assert_eq!(result.model_usage.cached_input_tokens, 70);
    assert_eq!(result.model_usage.reasoning_tokens, 7);
    assert_eq!(result.provider_attempts.attempt_count, 3);
    assert_eq!(result.provider_attempts.retry_count, 1);
    assert_eq!(result.provider_attempts.latency_ms, 105);
    let status = result.to_run_status();
    assert_eq!(status.model_usage, result.model_usage);
    assert_eq!(status.provider_attempts, result.provider_attempts);
}

#[test]
fn plan_tool_schema_matches_runtime_bounds() {
    let spec = agent_control_tool_entries()
        .into_iter()
        .next()
        .expect("plan tool entry")
        .spec;
    assert_eq!(spec.name, "update_plan");
    assert_eq!(spec.input_schema["properties"]["steps"]["minItems"], 1);
    assert_eq!(spec.input_schema["properties"]["steps"]["maxItems"], 64);
    assert_eq!(
        spec.input_schema["properties"]["steps"]["items"]["properties"]["step"]["maxLength"],
        512
    );
    assert_eq!(spec.input_schema["additionalProperties"], false);
}

#[test]
fn public_plan_and_tool_result_redact_sensitive_step_text() {
    let sensitive = "Authorization: Bearer sk-abcdefghijklmnopqrstuvwxyz123456";
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    plan_response.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": sensitive, "status": "completed"}]),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let result = agent_loop_with_plan_capabilities(
        vec![plan_response, final_response],
        allow_read_policy(),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract::default(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(
        result.plan.as_ref().unwrap().steps[0].step,
        "[redacted plan step]"
    );
    let serialized = serde_json::to_string(&result).expect("serialize result");
    assert!(!serialized.contains(sensitive));
    assert_eq!(
        result.tool_results[0].to_message_payload()["content"]["plan"]["steps"][0]["step"],
        "[redacted plan step]"
    );
}

#[test]
fn incomplete_plan_rejects_final_until_every_step_is_completed() {
    let mut initial_plan = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    initial_plan.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([
            {"step": "inspect", "status": "in_progress"},
            {"step": "verify", "status": "pending"}
        ]),
    ));
    let premature_final =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "not yet");
    let mut completed_plan =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    completed_plan.tool_calls.push(plan_tool_call(
        "plan_call_2",
        serde_json::json!([
            {"step": "inspect", "status": "completed"},
            {"step": "verify", "status": "completed"}
        ]),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_plan_capabilities(
        vec![
            initial_plan,
            premature_final,
            completed_plan,
            final_response,
        ],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "finish the plan").with_max_turns(4));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.plan_update_count, 2);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
    assert!(result.plan.as_ref().is_some_and(|plan| plan.is_completed()));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[2].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[3].tool_choice.mode, ToolChoiceMode::Auto);
    assert!(requests[2].messages.iter().any(|message| {
        message.role == ModelRole::Developer && message.content.contains("Complete every plan step")
    }));
}

#[test]
fn verified_completed_plan_enters_tool_free_finalization() {
    let workspace = tempfile::tempdir().expect("workspace");
    let bound_cwd = ".";
    let verification_argv = test_command("verify");
    let verification_digest = command_script_scope_digest_with_policy(
        &verification_argv.join(" "),
        bound_cwd,
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let mut initial_plan = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    initial_plan.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([
            {"step": "inspect", "status": "completed"},
            {"step": "verify", "status": "in_progress"}
        ]),
    ));
    let mut completed_plan =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    completed_plan.tool_calls.push(plan_tool_call(
        "plan_call_2",
        serde_json::json!([
            {"step": "inspect", "status": "completed"},
            {"step": "verify", "status": "completed"}
        ]),
    ));
    let mut verification = ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    verification.tool_calls.push(tool_call(
        "command_call_1",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut repeated_verification =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "");
    repeated_verification.tool_calls.push(tool_call(
        "command_call_repeated",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_final", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = AgentLoop::new(
        FinalizationAwareProvider {
            setup_responses: vec![initial_plan, completed_plan, verification],
            repeated_tool_response: repeated_verification,
            final_response: Ok(final_response),
            cancel_on_finalization: false,
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract {
                supports_required_tool_choice: true,
                ..ProviderProtocolContract::default()
            },
        },
        agent_tool_broker_for_test(true),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path()).with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new("thread_1", "turn_1", "finish and verify")
            .with_max_turns(3)
            .with_verification_requirements([AgentVerificationRequirement::new(
                verification_digest,
                1,
            )]),
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.model_turns, 4);
    assert_eq!(result.model_turn_limit, 3);
    assert!(result.verification.passed);
    assert!(result.plan.as_ref().is_some_and(AgentPlan::is_completed));
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|tool_result| tool_result.tool_name == "command")
            .count(),
        1
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 4);
    assert!(requests[..3].iter().all(|request| {
        request.tool_choice.mode == ToolChoiceMode::Auto && !request.tools.is_empty()
    }));
    assert_eq!(requests[3].tool_choice.mode, ToolChoiceMode::None);
    assert_eq!(requests[3].tool_choice.max_tool_calls, 0);
    assert!(!requests[3].tool_choice.strict_tool_schema);
    assert!(requests[3].tools.is_empty());
}

#[test]
fn terminal_finalization_failures_are_fail_closed_and_side_effect_free() {
    #[derive(Clone, Copy)]
    enum FinalizationCase {
        ProviderError,
        EmptyResponse,
        StructuredToolCall,
        Cancelled,
    }

    for case in [
        FinalizationCase::ProviderError,
        FinalizationCase::EmptyResponse,
        FinalizationCase::StructuredToolCall,
        FinalizationCase::Cancelled,
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        let bound_cwd = ".";
        let verification_argv = test_command("verify");
        let verification_digest = command_script_scope_digest_with_policy(
            &verification_argv.join(" "),
            bound_cwd,
            5,
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
        );
        let response_with_accounting = |request_id: &str,
                                        response_id: &str,
                                        content: &str,
                                        input_tokens: u64,
                                        output_tokens: u64,
                                        total_tokens: u64,
                                        latency_ms: u64| {
            let mut response = ModelTurnResponse::completed(request_id, response_id, content);
            response.usage = ModelUsage {
                input_tokens,
                output_tokens,
                total_tokens,
                ..ModelUsage::default()
            };
            response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
                attempt_count: 1,
                retry_count: 0,
                latency_ms,
            });
            response
        };

        let mut verification =
            response_with_accounting("model_request_turn_1_0", "response_1", "", 30, 3, 33, 30);
        verification.tool_calls.push(tool_call(
            "verify_call_1",
            "command",
            serde_json::json!({
                "command": verification_argv.join(" "),
                "cwd": ".",
                "timeout_seconds": 5
            }),
        ));

        let final_response = match case {
            FinalizationCase::ProviderError => Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::UnknownProviderError,
                    "terminal provider failed",
                )
                .with_provider_diagnostic(
                    "terminal_provider_failed",
                    singularity_model::ProviderErrorStage::RequestSend,
                ),
            )
            .with_provider_attempt_metadata(ProviderAttemptMetadata {
                attempt_count: 2,
                retry_count: 1,
                latency_ms: 40,
            })),
            FinalizationCase::EmptyResponse | FinalizationCase::Cancelled => {
                Ok(response_with_accounting(
                    "model_request_turn_1_1",
                    "response_final",
                    if matches!(case, FinalizationCase::Cancelled) {
                        "done"
                    } else {
                        ""
                    },
                    40,
                    4,
                    44,
                    40,
                ))
            }
            FinalizationCase::StructuredToolCall => {
                let mut response = response_with_accounting(
                    "model_request_turn_1_1",
                    "response_final",
                    "",
                    40,
                    4,
                    44,
                    40,
                );
                response.tool_calls.push(tool_call(
                    "terminal_call",
                    "command",
                    serde_json::json!({
                        "command": test_command_script("must-not-run"),
                        "timeout_seconds": 5
                    }),
                ));
                Ok(response)
            }
        };
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let cancellation = CancellationToken::new();
        let result = AgentLoop::new(
            FinalizationAwareProvider {
                setup_responses: vec![verification.clone()],
                repeated_tool_response: verification,
                final_response,
                cancel_on_finalization: matches!(case, FinalizationCase::Cancelled),
                seen_requests: Arc::clone(&seen_requests),
                capabilities: ProviderProtocolContract {
                    supports_required_tool_choice: true,
                    ..ProviderProtocolContract::default()
                },
            },
            agent_tool_broker_for_test(true),
            allow_read_execute_policy(),
        )
        .with_workspace_tools(
            WorkspaceTools::new(workspace.path()).with_sandbox_backend(AgentStrictBackend),
        )
        .with_cancellation_token(cancellation);

        let result = result.run(
            &AgentLoopInput::new("thread_1", "turn_1", "verify")
                .with_max_turns(1)
                .with_verification_requirements([AgentVerificationRequirement::new(
                    verification_digest,
                    1,
                )]),
        );

        assert_eq!(result.model_turns, 2);
        assert_eq!(result.model_turn_limit, 1);
        assert_ne!(result.status, AgentStatus::Completed);
        assert!(!result.completed);
        assert!(result.final_answer.is_none());
        assert!(result.verification.passed);
        assert_eq!(result.tool_calls, 1);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(
            result
                .tool_results
                .iter()
                .filter(|tool_result| tool_result.tool_name == "command")
                .count(),
            1
        );

        let requests = seen_requests.lock().expect("seen requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
        assert!(!requests[0].tools.is_empty());
        assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::None);
        assert_eq!(requests[1].tool_choice.max_tool_calls, 0);
        assert!(requests[1].tools.is_empty());
        drop(requests);

        match case {
            FinalizationCase::ProviderError => {
                assert_eq!(result.status, AgentStatus::Failed);
                assert_eq!(result.error.as_deref(), Some("terminal provider failed"));
                assert_eq!(
                    result.error_category,
                    Some(ModelErrorCategory::UnknownProviderError)
                );
                assert_eq!(result.model_usage.input_tokens, 30);
                assert_eq!(result.model_usage.output_tokens, 3);
                assert_eq!(result.model_usage.total_tokens, 33);
                assert_eq!(result.provider_attempts.attempt_count, 3);
                assert_eq!(result.provider_attempts.retry_count, 1);
                assert_eq!(result.provider_attempts.latency_ms, 70);
            }
            FinalizationCase::EmptyResponse => {
                assert_eq!(result.status, AgentStatus::Failed);
                assert_eq!(
                    result.error.as_deref(),
                    Some("model response validation failed: empty_response")
                );
                assert_eq!(result.error_category, Some(ModelErrorCategory::JsonSchema));
                let diagnostic = result
                    .provider_diagnostic
                    .as_ref()
                    .expect("typed empty response diagnostic");
                assert_eq!(
                    diagnostic.code.as_deref(),
                    Some("provider_response_invalid")
                );
                assert_eq!(diagnostic.validation_errors, ["empty_response"]);
                assert_eq!(result.model_usage.total_tokens, 77);
                assert_eq!(result.provider_attempts.attempt_count, 2);
                assert_eq!(result.provider_attempts.latency_ms, 70);
            }
            FinalizationCase::StructuredToolCall => {
                assert_eq!(result.status, AgentStatus::Failed);
                assert_eq!(
                    result.error.as_deref(),
                    Some(
                        "model response validation failed: max_tool_calls_exceeded, tool_choice_none"
                    )
                );
                assert_eq!(result.error_category, Some(ModelErrorCategory::JsonSchema));
                let diagnostic = result
                    .provider_diagnostic
                    .as_ref()
                    .expect("typed structured response diagnostic");
                assert_eq!(
                    diagnostic.code.as_deref(),
                    Some("provider_response_invalid")
                );
                assert_eq!(
                    diagnostic.validation_errors,
                    ["max_tool_calls_exceeded", "tool_choice_none"]
                );
                assert_eq!(result.recovery_metrics.invalid_tool_call_count, 1);
                assert_eq!(result.model_usage.total_tokens, 77);
                assert_eq!(result.provider_attempts.attempt_count, 2);
                assert_eq!(result.provider_attempts.latency_ms, 70);
            }
            FinalizationCase::Cancelled => {
                assert_eq!(result.status, AgentStatus::Cancelled);
                assert!(result.error.is_none());
                assert_eq!(result.model_usage.total_tokens, 77);
                assert_eq!(result.provider_attempts.attempt_count, 2);
                assert_eq!(result.provider_attempts.latency_ms, 70);
            }
        }
    }
}

#[test]
fn agent_loop_reports_all_unsatisfied_completion_invariants() {
    let mut initial_plan = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    initial_plan.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": "inspect", "status": "in_progress"}]),
    ));
    let premature_final =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "not yet");
    let plain_text =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "still working");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let input = AgentLoopInput::new("thread_1", "turn_1", "finish the plan")
        .with_max_turns(3)
        .with_verification_requirements([AgentVerificationRequirement::new(
            format!("sha256:{}", "0".repeat(64)),
            1,
        )]);
    let result = agent_loop_with_plan_capabilities(
        vec![initial_plan, premature_final, plain_text],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 2);
    assert_eq!(
        result.error.as_deref(),
        Some(
            "completion gate rejected final answer: plan has incomplete steps; completion gate rejected final answer: required verification commands are incomplete"
        )
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[2].tool_choice.mode, ToolChoiceMode::Auto);
    let feedback = requests[2]
        .messages
        .last()
        .expect("combined completion feedback");
    assert_eq!(feedback.role, ModelRole::Developer);
    assert!(feedback.content.contains("Complete every plan step"));
    assert!(feedback.content.contains("exact verification command"));
}

#[test]
fn repeated_invalid_calls_update_recovery_metrics_without_public_raw_arguments() {
    let invalid_arguments = serde_json::json!({
        "command": 17,
        "timeout_seconds": 5
    });
    let mut first_invalid =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first_invalid
        .tool_calls
        .push(tool_call("call_1", "command", invalid_arguments.clone()));
    let mut second_invalid =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    second_invalid
        .tool_calls
        .push(tool_call("call_2", "command", invalid_arguments));
    let mut successful_command =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    successful_command.tool_calls.push(tool_call(
        "call_3",
        "command",
        serde_json::json!({"command": test_command_script("success"), "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let workspace = tempfile::tempdir().expect("workspace");
    let result = agent_loop_with_capabilities(
        vec![
            first_invalid,
            second_invalid,
            successful_command,
            final_response,
        ],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path()).with_sandbox_backend(AgentStrictBackend),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "verify").with_max_turns(4));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.recovery_metrics.invalid_tool_call_count, 2);
    assert_eq!(result.recovery_metrics.repeated_tool_call_count, 1);
    assert_eq!(result.recovery_metrics.repair_attempt_count, 2);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 0);
    let requests = seen_requests.lock().expect("seen requests");
    assert!(requests[2].messages.iter().any(|message| {
        message.role == ModelRole::Developer
            && message
                .content
                .contains("same repairable tool failure recurred")
    }));
    let serialized = serde_json::to_string(&result).expect("serialize public result");
    assert!(!serialized.contains("raw_arguments"));
    assert!(!serialized.contains("sha256:"));
    assert!(!serialized.contains("not-an-array"));
}

#[test]
fn approval_resume_preserves_plan_and_recovery_metrics() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file_path = workspace.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    plan_response.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": "edit", "status": "completed"}]),
    ));
    plan_response.usage = ModelUsage {
        input_tokens: 10,
        output_tokens: 1,
        total_tokens: 11,
        ..ModelUsage::default()
    };
    plan_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        retry_count: 0,
        latency_ms: 10,
    });
    let mut edit_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    edit_response.tool_calls.push(tool_call(
        "edit_call_1",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    edit_response.usage = ModelUsage {
        input_tokens: 20,
        output_tokens: 2,
        total_tokens: 22,
        ..ModelUsage::default()
    };
    edit_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 2,
        retry_count: 1,
        latency_ms: 20,
    });
    let mut verify_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    verify_response.tool_calls.push(tool_call(
        "verify_call_1",
        "command",
        serde_json::json!({"command": test_command_script("success"), "timeout_seconds": 5}),
    ));
    verify_response.usage = ModelUsage {
        input_tokens: 30,
        output_tokens: 3,
        total_tokens: 33,
        ..ModelUsage::default()
    };
    verify_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        retry_count: 0,
        latency_ms: 30,
    });
    let mut final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    final_response.usage = ModelUsage {
        input_tokens: 40,
        output_tokens: 4,
        total_tokens: 44,
        ..ModelUsage::default()
    };
    final_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        retry_count: 0,
        latency_ms: 40,
    });
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = agent_loop_with_plan_capabilities(
        vec![
            plan_response,
            edit_response,
            verify_response,
            final_response,
        ],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path()).with_sandbox_backend(AgentStrictBackend),
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit").with_max_turns(3);
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(blocked.plan_update_count, 1);
    assert_eq!(blocked.recovery_metrics, AgentRecoveryMetrics::default());
    let pending = blocked.pending_tool_calls[0].clone();
    let checkpoint = blocked
        .approval_checkpoint(&pending.request_id)
        .expect("approval checkpoint");
    assert_eq!(checkpoint["plan"]["steps"][0]["status"], "completed");
    assert_eq!(checkpoint["plan_update_count"], 1);
    assert_eq!(checkpoint["recovery_metrics"]["repair_attempt_count"], 0);
    assert_eq!(checkpoint["model_usage"]["total_tokens"], 33);
    assert_eq!(checkpoint["provider_attempts"]["attempt_count"], 3);
    assert_eq!(checkpoint["provider_attempts"]["retry_count"], 1);
    assert!(checkpoint["seen_tool_call_fingerprints"].is_array());
    assert!(checkpoint["last_repair_failure"].is_null());

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.request_id.clone(),
        pending.tool_name.clone(),
        pending.resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_tool_call(&resumed_input, &pending, &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Completed);
    assert_eq!(resumed.model_turns, 4);
    assert_eq!(resumed.model_turn_limit, 3);
    assert_eq!(resumed.plan_update_count, 1);
    assert_eq!(resumed.recovery_metrics, AgentRecoveryMetrics::default());
    assert_eq!(resumed.model_usage.input_tokens, 100);
    assert_eq!(resumed.model_usage.output_tokens, 10);
    assert_eq!(resumed.model_usage.total_tokens, 110);
    assert_eq!(resumed.provider_attempts.attempt_count, 5);
    assert_eq!(resumed.provider_attempts.retry_count, 1);
    assert_eq!(resumed.provider_attempts.latency_ms, 100);
    assert!(
        resumed
            .plan
            .as_ref()
            .is_some_and(|plan| plan.is_completed())
    );
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 4);
    assert!(
        requests[..3]
            .iter()
            .all(|request| request.tool_choice.mode == ToolChoiceMode::Auto)
    );
    assert_eq!(requests[3].tool_choice.mode, ToolChoiceMode::None);
    assert!(requests[3].tools.is_empty());
}
