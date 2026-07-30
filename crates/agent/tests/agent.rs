//! AgentLoop 的 Direct tool、completion、approval 和恢复回归测试。

#![allow(clippy::needless_update)]

use sha2::{Digest, Sha256};
use singularity_agent::{
    AgentContextItem, AgentContextItemPriority, AgentLoop, AgentLoopEvent, AgentLoopEventSinkError,
    AgentLoopInput, AgentLoopResult, AgentObservation, AgentPlan, AgentPlanStep,
    AgentPlanStepStatus, AgentPlanUpdateInput, AgentRecoveryMetrics, AgentRepairReason,
    AgentRunStatus, AgentStatus, AgentVerificationAction, AgentVerificationCheck,
    AgentVerificationCommand, AgentVerificationEntry, AgentVerificationPlan,
    AgentVerificationRequirement, AgentVerificationRisk, ApprovalGrant, FinalReviewStatus,
    FinalReviewVerdict, OccurrenceLifecycle, PendingApprovalOccurrence, PolicyDecisionCause,
    PolicyDecisionStatus, PromptAssemblyStatus, RepairPlanningStatus, SandboxExecutionStatus,
    ToolCallStatus, TurnCheckpointPhase, VerificationPlanStatus, VerificationStatus,
    agent_control_tool_entries, assemble_context_items,
};
use singularity_core::{CancellationToken, ProjectInstructions, load_project_instructions};
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, ModelError, ModelErrorCategory, ModelErrorKind, ModelMessage,
    ModelPreferences, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, PROVIDER_STREAMING_UNSUPPORTED_CODE, Provider,
    ProviderApiProtocol, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStatus, ProviderCapabilityCacheLookupResult,
    ProviderCapabilityCacheObservation, ProviderCapabilityMetadata, ProviderCapabilityProfile,
    ProviderError, ProviderErrorStage, ProviderProtocolContract, ProviderProtocolNegotiation,
    ProviderStreamEvent, ToolChoiceMode,
};
use singularity_policy::{
    CommandScopeDigest, NetworkAccess, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionResource, PermissionRule, PolicyEngine, SettingsScope, ToolId,
    WorkspaceRelativePath,
};
use singularity_tools::{
    CommandRequest, CommandResult, CommandScriptRequest, SandboxBackend, SandboxCapabilities,
    SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolFailureKind, ToolRegistry,
    WorkspaceChangeSummary, WorkspaceMutation, WorkspaceTools,
    command_script_scope_digest_with_policy, workspace_tool_entries,
};
use std::path::PathBuf;
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

fn pending_approval(result: &AgentLoopResult) -> PendingApprovalOccurrence {
    result
        .pending_approvals
        .first()
        .expect("pending approval")
        .clone()
}

fn provider_attempt_occurrence(
    attempt_index: u32,
    provider_name: &str,
    terminal_status: ProviderAttemptStatus,
) -> ProviderAttemptOccurrence {
    ProviderAttemptOccurrence {
        operation_phase: ProviderAttemptOperationPhase::Completion,
        provider_name: provider_name.to_string(),
        model_name: "gpt-test".to_string(),
        actual_api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
        attempt_index,
        terminal_status,
        started_at_unix_ms: 1,
        ended_at_unix_ms: 2,
        attempt_duration_ms: 10,
        request_send_to_headers_ms: Some(4),
        queue_duration_ms: None,
        time_to_first_text_delta_ms: None,
        retry_scheduled: false,
        retry_backoff_ms: None,
        error_category: None,
        error_stage: None,
        diagnostic_code: None,
        usage: Some(ModelUsage::default()),
        model_turn_ordinal: None,
        parent_occurrence_id: None,
    }
}

struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
}

struct StreamingProvider {
    responses: Vec<(Vec<ProviderStreamEvent>, ModelTurnResponse)>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
}

struct DeltaThenUnsupportedProvider {
    fallback_calls: Arc<AtomicUsize>,
}

struct FinalizationStreamProvider {
    setup_response: ModelTurnResponse,
    final_events: Vec<ProviderStreamEvent>,
    final_response: Result<ModelTurnResponse, ProviderError>,
    cancel_on_finalization: bool,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

struct FinalizationAwareProvider {
    setup_responses: Vec<ModelTurnResponse>,
    repeated_tool_response: ModelTurnResponse,
    final_response: Result<ModelTurnResponse, ProviderError>,
    cancel_on_finalization: bool,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    capabilities: ProviderProtocolContract,
}

// Keep existing provider fixtures focused on the answer text while exercising the production
// typed final-review parser. Real providers must return this object themselves.
fn typed_fixture_final_review(
    request: &ModelTurnRequest,
    mut response: ModelTurnResponse,
) -> ModelTurnResponse {
    if request.tool_choice.mode != ToolChoiceMode::None || !request.tools.is_empty() {
        return response;
    }
    let Some(message) = response.assistant_message.as_ref() else {
        return response;
    };
    if message.content.trim().is_empty() {
        return response;
    }
    if serde_json::from_str::<serde_json::Value>(&message.content).is_ok() {
        return response;
    }
    let Some(instruction) = request.messages.iter().rev().find(|message| {
        message.role == ModelRole::Developer
            && message
                .content
                .contains("Return exactly one JSON object for the terminal review")
    }) else {
        return response;
    };
    let Some(template) = instruction
        .content
        .split_once("with no markdown: ")
        .and_then(|(_, value)| value.split_once(". The revision").map(|(value, _)| value))
    else {
        return response;
    };
    let (verdict, final_answer, reason) = match message.content.as_str() {
        "__fixture_review_repair__" => ("repair", "", "semantic contract remains incomplete"),
        "__fixture_review_reject__" => ("reject", "", "semantic contract remains incomplete"),
        answer => ("accept", answer, ""),
    };
    let template = template.replace("accept|reject|repair", verdict);
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&template) else {
        return response;
    };
    value["final_answer"] = serde_json::json!(final_answer);
    value["reason"] = serde_json::json!(reason);
    if let Ok(content) = serde_json::to_string(&value) {
        response.assistant_message = Some(ModelMessage::text(ModelRole::Assistant, content));
    }
    response
}

fn stream_fixture_response(
    request: &ModelTurnRequest,
    events: &[ProviderStreamEvent],
    response: ModelTurnResponse,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
) -> ModelTurnResponse {
    let typed = typed_fixture_final_review(request, response.clone());
    let original_text = response
        .assistant_message
        .as_ref()
        .map(|message| message.content.as_str());
    let typed_text = typed
        .assistant_message
        .as_ref()
        .map(|message| message.content.as_str());
    if typed_text != original_text {
        if let Some(delta) = typed_text {
            on_event(ProviderStreamEvent::OutputTextDelta {
                delta: delta.to_string(),
            });
        }
    } else {
        for event in events {
            on_event(event.clone());
        }
    }
    typed
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
            return self
                .final_response
                .clone()
                .map(|response| typed_fixture_final_review(request, response));
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
        Ok(typed_fixture_final_review(
            request,
            self.responses
                .get(response_index)
                .unwrap_or_else(|| self.responses.last().expect("static provider response"))
                .clone(),
        ))
    }
}

impl Provider for StreamingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.capabilities.clone()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        let (events, response) = self
            .responses
            .get(response_index)
            .unwrap_or_else(|| self.responses.last().expect("streaming provider response"));
        Ok(stream_fixture_response(
            request,
            events,
            response.clone(),
            on_event,
        ))
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        panic!("streaming provider must not use non-stream completion")
    }
}

impl Provider for DeltaThenUnsupportedProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        on_event(ProviderStreamEvent::OutputTextDelta {
            delta: "untrusted partial".to_string(),
        });
        Err(ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::UnsupportedCapability,
                "streaming is unsupported after output",
            )
            .with_provider_diagnostic(
                PROVIDER_STREAMING_UNSUPPORTED_CODE,
                ProviderErrorStage::ResponseValidation,
            ),
        ))
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.fallback_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "fallback_response",
            "fallback answer",
        ))
    }
}

impl Provider for FinalizationStreamProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        seen_requests.push(request.clone());
        let finalization_only =
            request.tool_choice.mode == ToolChoiceMode::None && request.tools.is_empty();
        if !finalization_only {
            return Ok(self.setup_response.clone());
        }
        if self.cancel_on_finalization {
            for event in &self.final_events {
                on_event(event.clone());
            }
            cancellation.cancel();
        }
        match self.final_response.clone() {
            Ok(response) if !self.cancel_on_finalization => Ok(stream_fixture_response(
                request,
                &self.final_events,
                response,
                on_event,
            )),
            Ok(response) => Ok(typed_fixture_final_review(request, response)),
            Err(error) => {
                for event in &self.final_events {
                    on_event(event.clone());
                }
                Err(error)
            }
        }
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        panic!("finalization stream provider must not use non-stream completion")
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
        Ok(typed_fixture_final_review(
            request,
            self.responses
                .get(response_index)
                .unwrap_or_else(|| {
                    self.responses
                        .last()
                        .expect("negotiating provider response")
                })
                .clone(),
        ))
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
            ..Default::default()
        },
        cache_observations: Vec::new(),
    }
}

fn runtime_negotiation_metadata() -> ProviderCapabilityMetadata {
    let mut metadata = negotiated_capability_metadata();
    metadata.cache_observations = vec![ProviderCapabilityCacheObservation {
        api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
        outcome: ProviderCapabilityCacheLookupResult::Miss,
        observed_at_unix_ms: 1,
        model_turn_ordinal: None,
        parent_occurrence_id: None,
    }];
    metadata.probe_attempt_metadata.occurrences = vec![provider_attempt_occurrence(
        1,
        "provider-probe",
        ProviderAttemptStatus::Ok,
    )];
    metadata
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
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR")).expect("bind workspace tools"),
    )
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
    let metadata = runtime_negotiation_metadata();
    let mut events = Vec::new();
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

    let result = agent_loop.run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "inspect"),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(negotiation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.provider_protocol_contract, Some(negotiated_contract));
    let prompt_id = events
        .iter()
        .find_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::PromptAssembly(observation))
                if matches!(observation.lifecycle, OccurrenceLifecycle::Started { .. }) =>
            {
                Some(observation.identity.occurrence_id.clone())
            }
            _ => None,
        })
        .expect("PromptAssembly start");
    let capability = result
        .provider_capability_metadata
        .as_ref()
        .expect("negotiation metadata");
    assert_eq!(capability.cache_observations.len(), 1);
    assert_eq!(
        capability.cache_observations[0]
            .parent_occurrence_id
            .as_deref(),
        Some(prompt_id.as_str())
    );
    assert_eq!(
        capability.probe_attempt_metadata.occurrences[0]
            .parent_occurrence_id
            .as_deref(),
        Some(prompt_id.as_str())
    );
    assert_eq!(capability.cache_observations[0].model_turn_ordinal, Some(0));
    assert_eq!(
        capability.probe_attempt_metadata.occurrences[0].model_turn_ordinal,
        Some(0)
    );
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
fn capability_negotiation_failure_preserves_provider_attempt_evidence_without_model_call() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let attempts = (1..=3)
        .map(|attempt_index| {
            let mut occurrence = provider_attempt_occurrence(
                attempt_index,
                &format!("capability-probe-{attempt_index}"),
                ProviderAttemptStatus::Error,
            );
            occurrence.operation_phase = ProviderAttemptOperationPhase::CapabilityProbe;
            occurrence.error_category = Some(ModelErrorCategory::UnsupportedCapability);
            occurrence.error_stage = Some(ProviderErrorStage::ResponseValidation);
            occurrence.diagnostic_code = Some("capability_negotiation_failed".to_string());
            occurrence.retry_scheduled = attempt_index < 3;
            occurrence.retry_backoff_ms = (attempt_index < 3).then_some(u64::from(attempt_index));
            occurrence
        })
        .collect();
    let metadata = ProviderAttemptMetadata {
        attempt_count: 3,
        retry_count: 2,
        latency_ms: 12,
        occurrences: attempts,
    };
    let error = ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "capability negotiation failed",
        )
        .with_provider_diagnostic(
            "capability_negotiation_failed",
            ProviderErrorStage::ResponseValidation,
        ),
    )
    .with_provider_attempt_metadata(metadata);
    let agent_loop = AgentLoop::new(
        NegotiatingProvider {
            responses: vec![ModelTurnResponse::completed(
                "request_1",
                "response_1",
                "must not be used",
            )],
            seen_requests: Arc::clone(&seen_requests),
            negotiation_calls: Arc::new(AtomicUsize::new(0)),
            static_capabilities: ProviderProtocolContract::default(),
            negotiated_capabilities: Err(error),
        },
        workspace_tool_broker_for_test(),
        allow_read_policy(),
    );

    let result = agent_loop.run(&AgentLoopInput::new("thread_1", "turn_1", "inspect"));

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(seen_requests.lock().expect("seen requests").is_empty());
    assert_eq!(result.model_turns, 0);
    assert_eq!(result.tool_calls, 0);
    assert_eq!(result.provider_attempts.attempt_count, 3);
    assert_eq!(result.provider_attempts.retry_count, 2);
    assert_eq!(result.provider_attempts.latency_ms, 12);
    assert_eq!(result.provider_attempts.occurrences.len(), 3);
    assert!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .all(|occurrence| {
                occurrence.operation_phase == ProviderAttemptOperationPhase::CapabilityProbe
                    && occurrence.model_turn_ordinal == Some(0)
                    && occurrence.parent_occurrence_id.is_none()
            })
    );
    assert_eq!(
        result.to_run_status().provider_attempts,
        result.provider_attempts
    );
}

#[test]
fn capability_negotiation_error_does_not_bind_an_unemitted_prompt_parent() {
    let metadata = runtime_negotiation_metadata();
    let error = ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "capability negotiation failed",
        )
        .with_provider_diagnostic(
            "capability_negotiation_failed",
            singularity_model::ProviderErrorStage::ResponseValidation,
        ),
    )
    .with_capability_metadata(metadata);
    let mut events = Vec::new();
    let result = AgentLoop::new(
        NegotiatingProvider {
            responses: vec![ModelTurnResponse::completed(
                "request_1",
                "response_1",
                "unused",
            )],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            negotiation_calls: Arc::new(AtomicUsize::new(0)),
            static_capabilities: ProviderProtocolContract::default(),
            negotiated_capabilities: Err(error),
        },
        workspace_tool_broker_for_test(),
        allow_read_policy(),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_pre_request", "turn_pre_request", "inspect"),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert!(events.iter().all(|event| {
        !matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::PromptAssembly(_))
        )
    }));
    let metadata = result
        .provider_capability_metadata
        .expect("negotiation metadata");
    assert!(
        metadata
            .cache_observations
            .iter()
            .all(|observation| observation.model_turn_ordinal == Some(0)
                && observation.parent_occurrence_id.is_none())
    );
    assert!(
        metadata
            .probe_attempt_metadata
            .occurrences
            .iter()
            .all(|occurrence| occurrence.model_turn_ordinal == Some(0)
                && occurrence.parent_occurrence_id.is_none())
    );
}

#[test]
fn pre_request_context_failure_does_not_bind_an_unemitted_prompt_parent() {
    let metadata = runtime_negotiation_metadata();
    let mut events = Vec::new();
    let result = AgentLoop::new(
        NegotiatingProvider {
            responses: vec![ModelTurnResponse::completed(
                "request_1",
                "response_1",
                "unused",
            )],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            negotiation_calls: Arc::new(AtomicUsize::new(0)),
            static_capabilities: ProviderProtocolContract::default(),
            negotiated_capabilities: Ok(ProviderProtocolNegotiation {
                contract: ProviderProtocolContract::default(),
                metadata,
            }),
        },
        workspace_tool_broker_for_test(),
        allow_read_policy(),
    )
    .run_with_events(
        &AgentLoopInput::new(
            "thread_pre_request",
            "turn_context_failure",
            "a".repeat(DEFAULT_MAX_CONTEXT_TOKENS as usize * 4 + 1),
        ),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::PromptAssembly(_))
        )
    }));
    let metadata = result
        .provider_capability_metadata
        .expect("negotiation metadata");
    assert!(
        metadata
            .cache_observations
            .iter()
            .all(|observation| observation.model_turn_ordinal == Some(0)
                && observation.parent_occurrence_id.is_none())
    );
    assert!(
        metadata
            .probe_attempt_metadata
            .occurrences
            .iter()
            .all(|occurrence| occurrence.model_turn_ordinal == Some(0)
                && occurrence.parent_occurrence_id.is_none())
    );
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
        serde_json::json!({
            "command": test_command_script("success"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
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
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit").with_max_turns(3);
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(negotiation_calls.load(Ordering::SeqCst), 1);
    let pending = pending_approval(&blocked);
    let steered_checkpoint = pending
        .checkpoint()
        .into_turn_checkpoint(&["use a different implementation".to_string()], true, &[])
        .expect("approval steer handoff")
        .encode()
        .expect("turn checkpoint");
    assert_eq!(
        steered_checkpoint["pending_tool_calls"],
        serde_json::json!([])
    );
    assert_eq!(
        steered_checkpoint["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("user message")["role"],
        "user"
    );
    assert!(
        steered_checkpoint["tool_result_occurrences"]
            .as_array()
            .expect("tool results")
            .iter()
            .any(|occurrence| {
                occurrence["result"]["error_code"] == "not_executed_due_to_user_input"
                    && occurrence["result"]["failure_kind"] == "cancelled"
            })
    );

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_approval(&resumed_input, &pending);

    assert_eq!(resumed.status, AgentStatus::Completed, "{resumed:?}");
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

fn verification_command(
    command: impl Into<String>,
    required_success_count: u32,
) -> AgentVerificationCommand {
    AgentVerificationCommand::new(
        AgentVerificationAction {
            command: command.into(),
            cwd: ".".to_string(),
            timeout_seconds: 5,
            sandbox_mode: SandboxFilesystemMode::WorkspaceWrite,
            network_access: SandboxNetworkMode::Denied,
        },
        required_success_count,
    )
}

fn finalization_stream_fixture() -> (AgentLoopInput, ModelTurnResponse) {
    let verification_argv = test_command("verify");
    let mut setup_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_tool", "");
    let call = tool_call(
        "verify_call",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    );
    setup_response.tool_calls = vec![call.clone()];
    setup_response.assistant_message = Some(ModelMessage {
        role: ModelRole::Assistant,
        content: String::new(),
        tool_call_id: None,
        tool_calls: vec![call],
    });
    (
        AgentLoopInput::new("thread_1", "turn_1", "verify")
            .with_max_turns(2)
            .with_verification_commands([verification_command(verification_argv.join(" "), 1)]),
        setup_response,
    )
}

fn finalization_stream_agent(
    provider: FinalizationStreamProvider,
) -> AgentLoop<FinalizationStreamProvider> {
    AgentLoop::new(
        provider,
        agent_tool_broker_for_test(false),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR"))
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
}

fn plan_tool_call(id: &str, steps: serde_json::Value) -> ModelToolCall {
    tool_call(id, "update_plan", serde_json::json!({"steps": steps}))
}

fn workspace_edit_response(
    request: &str,
    response: &str,
    call_id: &str,
    expected: &str,
    replacement: &str,
) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed(request, response, "");
    response.tool_calls.push(tool_call(
        call_id,
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": expected,
            "replacement": replacement
        }),
    ));
    response
}

fn workspace_command_response(
    request: &str,
    response: &str,
    call_id: &str,
    argument: &str,
) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed(request, response, "");
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
}

fn workspace_verification_plan_response(
    request: &str,
    response: &str,
    call_id: &str,
    argument: &str,
) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed(request, response, "");
    response.tool_calls.push(tool_call(
        call_id,
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the changed fixture", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "changed README.md",
                "affected_path": "README.md",
                "affected_symbol": "README.md::fixture_boundary",
                "current_gap": "verification evidence is not yet recorded",
                "action": {
                    "command": test_command_script(argument),
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    response
}

fn workspace_verification_plan_response_with_commands(
    request: &str,
    response: &str,
    call_id: &str,
    arguments: &[&str],
) -> ModelTurnResponse {
    let verification = arguments
        .iter()
        .enumerate()
        .map(|(index, argument)| {
            serde_json::json!({
                "risk": "general_mutation",
                "evidence": "changed README.md",
                "affected_path": "README.md",
                "affected_symbol": format!("README.md::fixture_boundary_{index}"),
                "current_gap": "verification evidence is not yet recorded",
                "action": {
                    "command": test_command_script(argument),
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            })
        })
        .collect::<Vec<_>>();
    let mut response = ModelTurnResponse::completed(request, response, "");
    response.tool_calls.push(tool_call(
        call_id,
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the changed fixture", "status": "completed"}],
            "verification": verification
        }),
    ));
    response
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
fn event_aware_run_reports_safe_prompt_assembly_at_the_request_boundary() {
    let secret = "sk-secret-prompt-value";
    let input = AgentLoopInput::new("thread_1", "turn_1", secret);
    let mut events = Vec::new();
    let result = agent_loop_with_response(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
    )
    .run_with_events(&input, &mut |event| {
        events.push(event);
        Ok(())
    });

    assert_eq!(result.status, AgentStatus::Completed);
    let prompt_events = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::PromptAssembly(prompt)) => Some(prompt),
            AgentLoopEvent::FinalTextDelta { .. } | AgentLoopEvent::Observation(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(prompt_events.len(), 2);
    assert_eq!(
        prompt_events[0].identity.occurrence_id,
        prompt_events[1].identity.occurrence_id
    );
    assert!(matches!(
        prompt_events[0].lifecycle,
        OccurrenceLifecycle::Started { .. }
    ));
    let OccurrenceLifecycle::Finished {
        status: PromptAssemblyStatus::Ready,
        ..
    } = prompt_events[1].lifecycle
    else {
        panic!("prompt assembly must finish ready");
    };
    assert_eq!(prompt_events[1].model_turn_ordinal, 0);
    assert!(!prompt_events[1].finalization_only);
    assert!(prompt_events[1].message_count > 0);
    assert!(prompt_events[1].tool_count > 0);
    assert!(prompt_events[1].request_token_count > 0);
    assert!(prompt_events[1].request_digest.starts_with("sha256:"));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::FinalReview(_))
    )));
    assert!(
        !serde_json::to_string(&events)
            .expect("serialize events")
            .contains(secret)
    );
}

#[test]
fn event_sink_failure_is_sanitized_and_stops_before_the_next_side_effect() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "unused"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "hello"),
        &mut |_| Err(AgentLoopEventSinkError),
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("agent event sink failed"));
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn agent_loop_aggregates_stream_deltas_and_requires_matching_terminal_text() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = AgentLoop::new(
        StreamingProvider {
            responses: vec![(
                vec![
                    ProviderStreamEvent::OutputTextDelta {
                        delta: "hel".to_string(),
                    },
                    ProviderStreamEvent::OutputTextDelta {
                        delta: "lo".to_string(),
                    },
                ],
                ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "hello"),
            )],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "hello"));

    assert_eq!(agent_loop.status, AgentStatus::Completed);
    assert_eq!(agent_loop.final_answer.as_deref(), Some("hello"));
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 1);
}

#[test]
fn agent_loop_nonstream_fallback_keeps_text_callback_empty() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut deltas = Vec::new();
    let result = agent_loop_with_response_and_requests(
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done"),
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run_with_text_deltas(
        &AgentLoopInput::new("thread_1", "turn_1", "hello"),
        &mut |delta| deltas.push(delta.to_string()),
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert!(deltas.is_empty());
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 1);
}

#[test]
fn agent_loop_rejects_streaming_unsupported_after_a_text_delta_without_fallback() {
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let mut deltas = Vec::new();
    let result = AgentLoop::new(
        DeltaThenUnsupportedProvider {
            fallback_calls: Arc::clone(&fallback_calls),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .run_with_text_deltas(
        &AgentLoopInput::new("thread_1", "turn_1", "hello"),
        &mut |delta| deltas.push(delta.to_string()),
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(deltas.is_empty());
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        result
            .provider_diagnostic
            .and_then(|diagnostic| diagnostic.code),
        Some(PROVIDER_STREAMING_UNSUPPORTED_CODE.to_string())
    );
}

#[test]
fn agent_loop_fails_closed_when_streamed_text_differs_from_terminal_text() {
    let agent_loop = AgentLoop::new(
        StreamingProvider {
            responses: vec![(
                vec![ProviderStreamEvent::OutputTextDelta {
                    delta: "hullo".to_string(),
                }],
                ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "hello"),
            )],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "hello"));

    assert_eq!(agent_loop.status, AgentStatus::Failed);
    assert_eq!(agent_loop.model_turns, 1);
    assert_eq!(
        agent_loop
            .provider_diagnostic
            .and_then(|diagnostic| diagnostic.code),
        Some("provider_stream_text_mismatch".to_string())
    );
}

#[test]
fn agent_loop_does_not_project_stream_deltas_before_response_validation() {
    let mut invalid =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "hidden");
    invalid.response_id.clear();
    let mut deltas = Vec::new();
    let result = AgentLoop::new(
        StreamingProvider {
            responses: vec![(
                vec![ProviderStreamEvent::OutputTextDelta {
                    delta: "hidden".to_string(),
                }],
                invalid,
            )],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .run_with_text_deltas(
        &AgentLoopInput::new("thread_1", "turn_1", "hello"),
        &mut |delta| deltas.push(delta.to_string()),
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(deltas.is_empty());
    assert_eq!(
        result
            .provider_diagnostic
            .and_then(|diagnostic| diagnostic.code),
        Some("provider_response_invalid".to_string())
    );
}

#[test]
fn agent_loop_projects_only_finalization_text_deltas_in_order() {
    let verification_argv = test_command("verify");
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_tool", "intermediate");
    let call = tool_call(
        "verify_call",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    );
    tool_response.tool_calls = vec![call.clone()];
    tool_response.assistant_message = Some(ModelMessage {
        role: ModelRole::Assistant,
        content: "intermediate".to_string(),
        tool_call_id: None,
        tool_calls: vec![call],
    });
    tool_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        occurrences: vec![provider_attempt_occurrence(
            1,
            "provider-setup",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    tool_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Miss,
            observed_at_unix_ms: 2,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });
    let mut final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_final", "done");
    final_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        occurrences: vec![provider_attempt_occurrence(
            2,
            "provider-finalization",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    final_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Hit,
            observed_at_unix_ms: 3,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut deltas = Vec::new();
    let mut events = Vec::new();
    let result = AgentLoop::new(
        StreamingProvider {
            responses: vec![
                (
                    vec![ProviderStreamEvent::OutputTextDelta {
                        delta: "intermediate".to_string(),
                    }],
                    tool_response,
                ),
                (
                    vec![
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "do".to_string(),
                        },
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "ne".to_string(),
                        },
                    ],
                    final_response,
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR"))
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "verify")
            .with_max_turns(2)
            .with_verification_commands([verification_command(verification_argv.join(" "), 1)]),
        &mut |event| {
            if let AgentLoopEvent::FinalTextDelta { delta } = &event {
                deltas.push(delta.clone());
            }
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(deltas, ["done"]);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::None);
    assert!(requests[1].tools.is_empty());
    let prompt_ids = events.iter().fold(Vec::new(), |mut prompt_ids, event| {
        if let AgentLoopEvent::Observation(AgentObservation::PromptAssembly(observation)) = event
            && !prompt_ids.contains(&observation.identity.occurrence_id)
        {
            prompt_ids.push(observation.identity.occurrence_id.clone());
        }
        prompt_ids
    });
    assert_eq!(prompt_ids.len(), 2);
    assert_eq!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.model_turn_ordinal,
                occurrence.parent_occurrence_id.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(0), Some(prompt_ids[0].clone())),
            (Some(1), Some(prompt_ids[1].clone())),
        ]
    );
    let capability = result
        .provider_capability_metadata
        .as_ref()
        .expect("finalization capability observations");
    assert_eq!(
        capability
            .cache_observations
            .iter()
            .map(|observation| (
                observation.outcome,
                observation.model_turn_ordinal,
                observation.parent_occurrence_id.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                ProviderCapabilityCacheLookupResult::Miss,
                Some(0),
                Some(prompt_ids[0].clone()),
            ),
            (
                ProviderCapabilityCacheLookupResult::Hit,
                Some(1),
                Some(prompt_ids[1].clone()),
            ),
        ]
    );
}

#[test]
fn agent_loop_withholds_unvalidated_final_review_text_when_terminal_validation_fails() {
    let verification_argv = test_command("verify");
    let verification_digest = command_script_scope_digest_with_policy(
        &verification_argv.join(" "),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_tool", "intermediate");
    let call = tool_call(
        "verify_call",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    );
    tool_response.tool_calls = vec![call.clone()];
    tool_response.assistant_message = Some(ModelMessage {
        role: ModelRole::Assistant,
        content: "intermediate".to_string(),
        tool_call_id: None,
        tool_calls: vec![call],
    });
    let mismatched = ModelTurnResponse::completed(
        "model_request_turn_1_1",
        "response_final",
        serde_json::json!({
            "verdict": "accept",
            "workspace_revision": 0,
            "change_digest": null,
            "verification_digests": [verification_digest],
            "final_answer": "terminal",
            "reason": ""
        })
        .to_string(),
    );
    let mut deltas = Vec::new();
    let result = AgentLoop::new(
        StreamingProvider {
            responses: vec![
                (
                    vec![ProviderStreamEvent::OutputTextDelta {
                        delta: "intermediate".to_string(),
                    }],
                    tool_response,
                ),
                (
                    vec![ProviderStreamEvent::OutputTextDelta {
                        delta: "partial".to_string(),
                    }],
                    mismatched,
                ),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR"))
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_text_deltas(
        &AgentLoopInput::new("thread_1", "turn_1", "verify")
            .with_max_turns(2)
            .with_verification_commands([verification_command(verification_argv.join(" "), 1)]),
        &mut |delta| deltas.push(delta.to_string()),
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(deltas.is_empty());
    assert_eq!(
        result
            .provider_diagnostic
            .and_then(|diagnostic| diagnostic.code),
        Some("provider_stream_text_mismatch".to_string())
    );
    assert!(!result.completed);
}

#[test]
fn agent_loop_withholds_unvalidated_final_review_text_when_stream_fails() {
    let (input, setup_response) = finalization_stream_fixture();
    let error = ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "finalization stream failed",
        )
        .with_provider_diagnostic(
            "finalization_stream_failed",
            singularity_model::ProviderErrorStage::ResponseValidation,
        ),
    );
    let mut deltas = Vec::new();
    let result = finalization_stream_agent(FinalizationStreamProvider {
        setup_response,
        final_events: vec![ProviderStreamEvent::OutputTextDelta {
            delta: "partial".to_string(),
        }],
        final_response: Err(error),
        cancel_on_finalization: false,
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    })
    .run_with_text_deltas(&input, &mut |delta| deltas.push(delta.to_string()));

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(!result.completed);
    assert!(result.final_answer.is_none());
    assert!(deltas.is_empty());
    assert_eq!(
        result
            .provider_diagnostic
            .and_then(|diagnostic| diagnostic.code),
        Some("finalization_stream_failed".to_string())
    );
}

#[test]
fn agent_loop_withholds_unvalidated_final_review_text_when_cancelled() {
    let (input, setup_response) = finalization_stream_fixture();
    let late_terminal =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_late", "late terminal");
    let mut deltas = Vec::new();
    let result = finalization_stream_agent(FinalizationStreamProvider {
        setup_response,
        final_events: vec![ProviderStreamEvent::OutputTextDelta {
            delta: "partial".to_string(),
        }],
        final_response: Ok(late_terminal),
        cancel_on_finalization: true,
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    })
    .run_with_text_deltas(&input, &mut |delta| deltas.push(delta.to_string()));

    assert_eq!(result.status, AgentStatus::Cancelled);
    assert!(!result.completed);
    assert!(result.final_answer.is_none());
    assert!(deltas.is_empty());
}

#[test]
fn approval_resume_projects_finalization_text_deltas() {
    let verification_argv = test_command("verify");
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_tool", "approval needed");
    let call = tool_call(
        "verify_call",
        "command",
        serde_json::json!({
            "command": verification_argv.join(" "),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    );
    tool_response.tool_calls = vec![call.clone()];
    tool_response.assistant_message = Some(ModelMessage {
        role: ModelRole::Assistant,
        content: "approval needed".to_string(),
        tool_call_id: None,
        tool_calls: vec![call],
    });
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_final", "resumed");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = AgentLoop::new(
        StreamingProvider {
            responses: vec![
                (
                    vec![ProviderStreamEvent::OutputTextDelta {
                        delta: "approval needed".to_string(),
                    }],
                    tool_response,
                ),
                (
                    vec![
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "re".to_string(),
                        },
                        ProviderStreamEvent::OutputTextDelta {
                            delta: "sumed".to_string(),
                        },
                    ],
                    final_response,
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR"))
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "verify")
        .with_max_turns(2)
        .with_verification_commands([verification_command(verification_argv.join(" "), 1)]);
    let blocked = agent_loop.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    let pending = pending_approval(&blocked);
    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let mut deltas = Vec::new();
    let resumed = agent_loop.resume_pending_approval_with_text_deltas(
        &resumed_input,
        &pending,
        &mut |delta| deltas.push(delta.to_string()),
    );

    assert_eq!(resumed.status, AgentStatus::Completed);
    assert_eq!(resumed.final_answer.as_deref(), Some("resumed"));
    assert_eq!(deltas, ["resumed"]);
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 2);
}

#[test]
fn agent_loop_executes_only_tool_calls_from_stream_terminal_envelope() {
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_tool", "");
    let call = tool_call(
        "call_read",
        "read",
        serde_json::json!({"path": "Cargo.toml"}),
    );
    tool_response.tool_calls = vec![call.clone()];
    tool_response.assistant_message = Some(ModelMessage {
        role: ModelRole::Assistant,
        content: String::new(),
        tool_call_id: None,
        tool_calls: vec![call],
    });
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = AgentLoop::new(
        StreamingProvider {
            responses: vec![
                (Vec::new(), tool_response),
                (
                    vec![ProviderStreamEvent::OutputTextDelta {
                        delta: "done".to_string(),
                    }],
                    ModelTurnResponse::completed("model_request_turn_1_1", "response_done", "done"),
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR")).expect("bind workspace tools"),
    )
    .run(&AgentLoopInput::new(
        "thread_1",
        "turn_1",
        "read Cargo.toml",
    ));

    assert_eq!(agent_loop.status, AgentStatus::Completed);
    assert_eq!(agent_loop.final_answer.as_deref(), Some("done"));
    assert_eq!(agent_loop.tool_results.len(), 1);
    assert_eq!(agent_loop.tool_results[0].tool_call_id, "call_read");
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 2);
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

    let mut events = Vec::new();
    let result = agent_loop_with_responses_and_requests(
        vec![edit, final_response],
        policy,
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run_with_events(&input, &mut |event| {
        events.push(event);
        Ok(())
    });

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
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::Verification(value))
            if value.occurrence_count == 1
                && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: VerificationStatus::GateRejected,
                    ..
                })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::Verification(value))
            if value.occurrence_count == 1
                && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: VerificationStatus::RepairRequested,
                    ..
                })
    )));
}

#[test]
fn agent_loop_preserves_portable_unknown_tool_history_with_empty_arguments() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "ready").expect("write fixture");
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(3);
    let mut portable_unknown =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    portable_unknown.tool_calls.push(tool_call(
        "call_1",
        "run_tests",
        serde_json::json!({"unexpected": true}),
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
        vec![portable_unknown, repaired_response, final_response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
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
    assert_eq!(result.tool_results[0].tool_name, "run_tests");
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
    assert!(requests[1].tools.iter().any(|tool| tool.name == "read"));
    assert!(
        requests[1]
            .tools
            .iter()
            .all(|tool| tool.name != "run_tests")
    );
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
    assert_eq!(rejected_call.tool_name, "run_tests");
    assert_eq!(rejected_call.arguments, serde_json::json!({}));
    assert_eq!(rejected_call.raw_arguments, "{}");
    let assistant_position = requests[1]
        .messages
        .iter()
        .position(|message| message.role == ModelRole::Assistant && !message.tool_calls.is_empty())
        .expect("assistant tool call position");
    let tool_position = requests[1]
        .messages
        .iter()
        .position(|message| {
            message.role == ModelRole::Tool && message.tool_call_id.as_deref() == Some("call_1")
        })
        .expect("tool result position");
    assert!(assistant_position < tool_position);
    let tool_message = &requests[1].messages[tool_position];
    let payload: serde_json::Value =
        serde_json::from_str(&tool_message.content).expect("tool result payload");
    assert_eq!(payload["tool_name"], "run_tests");
    assert_eq!(payload["error_code"], "tool_not_visible");
    for field in [
        "visible_tool_names",
        "rejection_kind",
        "name_projection",
        "correction",
        "placeholder_non_callable",
    ] {
        assert!(
            payload["content"].get(field).is_none(),
            "unexpected field {field}"
        );
    }
    assert!(requests.iter().all(|request| {
        request
            .tools
            .iter()
            .all(|tool| tool.name != "tool_rejected")
            && request
                .messages
                .iter()
                .flat_map(|message| message.tool_calls.iter())
                .all(|call| call.tool_name != "tool_rejected")
    }));
}

#[test]
fn agent_loop_rejects_nonportable_provider_tool_name_before_history() {
    let unsafe_name = "private/C:\\sensitive-tool";
    let unsafe_argument = "C:\\private\\credential.txt";
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_unsafe",
        unsafe_name,
        serde_json::json!({"path": unsafe_argument}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_responses_and_requests(
        vec![response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    assert!(result.tool_results.is_empty());
    assert_eq!(result.model_turns, 1);
    assert_eq!(
        result.error.as_deref(),
        Some("model response validation failed: tool_name_not_provider_portable")
    );
    let diagnostic = result
        .provider_diagnostic
        .as_ref()
        .expect("provider diagnostic");
    assert_eq!(
        diagnostic.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(
        diagnostic.validation_errors,
        vec!["tool_name_not_provider_portable".to_string()]
    );
    let serialized = serde_json::to_string(&result).expect("serialize public result");
    assert!(!serialized.contains(unsafe_name));
    assert!(!serialized.contains(unsafe_argument));
    assert!(!serialized.contains("tool_rejected"));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().all(|message| {
        message.tool_calls.is_empty() && !message.content.contains(unsafe_name)
    }));
    assert!(
        requests[0]
            .tools
            .iter()
            .all(|tool| tool.name != "tool_rejected")
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
    assert!(result.pending_approvals.is_empty());
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

    let mut events = Vec::new();
    let result = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .run_with_events(&input, &mut |event| {
        events.push(event);
        Ok(())
    });

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::PolicyDecision(value))
            if value.cause == Some(PolicyDecisionCause::NoMatchingRule)
                && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: PolicyDecisionStatus::Ask,
                    ..
                })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
            if matches!(value.lifecycle, OccurrenceLifecycle::Suspended {
                status: ToolCallStatus::ApprovalRequired,
                ..
            })
    )));
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
    let mut events = Vec::new();
    let mut checkpoint_events = Vec::new();
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run_with_events_and_checkpoints(
        &input,
        &mut |event| {
            events.push(event);
            Ok(())
        },
        &mut |event| {
            checkpoint_events.push(event);
            Ok(())
        },
    );

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
    let tool_events = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
                if value.model_turn_ordinal == 0 =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 6);
    assert!(
        tool_events[..3]
            .iter()
            .all(|value| matches!(value.lifecycle, OccurrenceLifecycle::Started { .. }))
    );
    assert_eq!(
        tool_events[3..]
            .iter()
            .map(|value| value.tool_call_ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::SandboxExecution(_))
    )));
    let committed_batches = checkpoint_events
        .iter()
        .filter_map(|event| match &event.phase {
            TurnCheckpointPhase::ToolResultsCommitted { tool_call_ids } => Some(tool_call_ids),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(committed_batches.len(), 1);
    assert_eq!(committed_batches[0], &["call_1", "call_2", "call_3"]);
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
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut events = Vec::new();
    let result = agent_loop_with_capabilities(
        vec![
            response,
            recovery,
            ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done"),
        ],
        allow_read_policy().with_rule(ask_readme),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "read files").with_max_turns(3),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert!(result.pending_approvals.is_empty());
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("tool_batch_rejected")
    );
    assert_eq!(
        result.tool_results[1].failure_kind,
        Some(singularity_tools::ToolFailureKind::Input)
    );
    let sibling_payload = result.tool_results[0].to_message_payload();
    assert_eq!(sibling_payload["content"]["batch_executed"], false);
    assert_eq!(sibling_payload["content"]["call_executed"], false);
    assert_eq!(
        sibling_payload["content"]["execution_mode"],
        "parallel_read"
    );
    assert_eq!(sibling_payload["content"]["trigger_tool_name"], "read");
    assert_eq!(
        sibling_payload["content"]["trigger_error_code"],
        "invalid_tool_arguments"
    );
    assert_eq!(
        sibling_payload["content"]["trigger_category"],
        "preflight_failure"
    );
    assert!(
        sibling_payload["content"]["required_next_action"]
            .as_str()
            .is_some_and(|value| value.contains("submit") && value.contains("alone"))
    );
    let rejected_payload = result.tool_results[1].to_message_payload();
    assert_eq!(rejected_payload["content"]["batch_executed"], false);
    assert_eq!(rejected_payload["content"]["call_executed"], false);
    assert_eq!(
        rejected_payload["content"]["safety_category"],
        "preflight_failure"
    );
    assert_eq!(
        rejected_payload["content"]["call_preflight_status"],
        "rejected"
    );
    assert_eq!(
        rejected_payload["content"]["validation_code"],
        "read_input_schema_mismatch"
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
    let batch_rejections = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
                    if value.model_turn_ordinal == 0
                        && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                            status: ToolCallStatus::BatchRejected,
                            ..
                        })
            )
        })
        .count();
    assert_eq!(batch_rejections, 2);
    let requests = seen_requests.lock().expect("seen requests");
    let tool_messages = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tool_messages.len(), 2);
    assert_eq!(tool_messages[0].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(tool_messages[1].tool_call_id.as_deref(), Some("call_2"));
    for message in tool_messages {
        let payload: serde_json::Value =
            serde_json::from_str(&message.content).expect("batch rejection payload");
        assert_eq!(payload["content"]["batch_executed"], false);
        assert_eq!(payload["content"]["call_executed"], false);
    }
}

#[test]
fn agent_loop_rejects_an_unsafe_batch_before_history_or_tool_results() {
    let unsafe_name = "private/C:\\sensitive-tool";
    let unsafe_argument = "C:\\private\\credential.txt";
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls.push(tool_call(
        "call_unsafe",
        unsafe_name,
        serde_json::json!({"path": unsafe_argument}),
    ));
    response.tool_calls.push(tool_call(
        "call_read",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));

    let result = agent_loop_with_capabilities(
        vec![response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read files").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    assert!(result.tool_results.is_empty());
    assert_eq!(result.model_turns, 1);
    assert_eq!(
        result.error.as_deref(),
        Some("model response validation failed: tool_name_not_provider_portable")
    );
    assert_eq!(
        result
            .provider_diagnostic
            .as_ref()
            .and_then(|diagnostic| diagnostic.code.as_deref()),
        Some("provider_response_invalid")
    );
    let serialized = serde_json::to_string(&result).expect("serialize public result");
    assert!(!serialized.contains(unsafe_name));
    assert!(!serialized.contains(unsafe_argument));
    assert!(!serialized.contains("tool_rejected"));
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().all(|message| {
        message.tool_calls.is_empty() && !message.content.contains(unsafe_name)
    }));
    assert!(
        requests[0]
            .tools
            .iter()
            .all(|tool| tool.name != "tool_rejected")
    );
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    let mutation_payload = result.tool_results[0].to_message_payload();
    assert_eq!(mutation_payload["content"]["batch_executed"], false);
    assert_eq!(mutation_payload["content"]["call_executed"], false);
    assert_eq!(mutation_payload["content"]["execution_mode"], "exclusive");
    assert_eq!(mutation_payload["content"]["trigger_tool_name"], "edit");
    assert_eq!(
        mutation_payload["content"]["trigger_error_code"],
        "exclusive_tool_requires_single_call"
    );
    assert_eq!(mutation_payload["content"]["trigger_category"], "exclusive");
    let sibling_payload = result.tool_results[1].to_message_payload();
    assert_eq!(sibling_payload["content"]["batch_executed"], false);
    assert_eq!(sibling_payload["content"]["call_executed"], false);
    assert_eq!(
        sibling_payload["content"]["execution_mode"],
        "parallel_read"
    );
    assert_eq!(sibling_payload["content"]["trigger_tool_name"], "edit");
    assert!(
        sibling_payload["content"]["required_next_action"]
            .as_str()
            .is_some_and(|value| value.contains("wait for its result"))
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read files").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert!(result.pending_approvals.is_empty());
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        result.tool_results[1].error_code.as_deref(),
        Some("tool_batch_rejected")
    );
    let approval_payload = result.tool_results[0].to_message_payload();
    assert_eq!(approval_payload["content"]["batch_executed"], false);
    assert_eq!(approval_payload["content"]["call_executed"], false);
    assert_eq!(
        approval_payload["content"]["execution_mode"],
        "parallel_read"
    );
    assert_eq!(
        approval_payload["content"]["trigger_error_code"],
        "approval_required"
    );
    let sibling_payload = result.tool_results[1].to_message_payload();
    assert_eq!(sibling_payload["content"]["batch_executed"], false);
    assert_eq!(sibling_payload["content"]["call_executed"], false);
    assert_eq!(
        sibling_payload["content"]["trigger_category"],
        "approval_sensitive"
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

    let mut duplicate_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    duplicate_response.tool_calls.push(tool_call(
        "same_call",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    duplicate_response.tool_calls.push(tool_call(
        "same_call",
        "read",
        serde_json::json!({"path": "CHANGELOG.md"}),
    ));
    let duplicate = agent_loop_with_response(duplicate_response, allow_read_policy()).run(&input);
    assert_eq!(duplicate.status, AgentStatus::Failed);
    assert!(duplicate.error.as_deref().is_some_and(|error| {
        error.contains("model response validation failed")
            && error.contains("duplicate_tool_call_id")
    }));
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

    let agent_loop = agent_loop_with_response(
        response,
        PolicyEngine::new(PermissionProfile::workspace_write()),
    );
    let result = agent_loop.run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    let pending = pending_approval(&result);
    assert_eq!(
        pending.pending_tool_call().request_id,
        "approval_turn_1_call_1"
    );
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    assert_eq!(checkpoint["checkpoint_version"], 3);
    assert_eq!(checkpoint["thread_id"], "thread_1");
    assert_eq!(checkpoint["turn_id"], "turn_1");
    assert_eq!(checkpoint["request_id"], "approval_turn_1_call_1");
    assert_eq!(checkpoint["tool_call_id"], "call_1");
    assert_eq!(checkpoint["approval_count"], 1);
    assert_eq!(checkpoint["model_turns"], 1);
    assert_eq!(checkpoint["used_approval_grants"], serde_json::json!([]));
    assert_eq!(checkpoint["tool_result_occurrences"], serde_json::json!([]));
    assert!(checkpoint.get("tool_result_context_bindings").is_none());
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

    let typed_roundtrip =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
            .expect("typed checkpoint roundtrip");
    assert_eq!(typed_roundtrip, pending);
    assert_eq!(
        checkpoint,
        pending.encode_checkpoint().expect("checkpoint roundtrip")
    );
    let mut future_checkpoint = checkpoint.clone();
    future_checkpoint["checkpoint_version"] = serde_json::json!(999);
    let future = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &future_checkpoint,
    );
    assert_eq!(
        future.expect_err("future checkpoint must fail"),
        "unsupported approval checkpoint version"
    );

    let mut unknown_field_checkpoint = checkpoint.clone();
    unknown_field_checkpoint["unexpected"] = serde_json::json!("must reject");
    let unknown_field = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &unknown_field_checkpoint,
    );
    assert!(
        unknown_field
            .expect_err("unknown checkpoint field must fail")
            .contains("unknown field")
    );

    let mut incomplete_checkpoint = checkpoint.clone();
    incomplete_checkpoint
        .as_object_mut()
        .expect("checkpoint object")
        .remove("tool_result_occurrences");
    let incomplete = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &incomplete_checkpoint,
    );
    assert!(
        incomplete
            .expect_err("incomplete checkpoint must fail")
            .contains("invalid approval checkpoint")
    );

    let mut missing_repair_ledger = checkpoint.clone();
    missing_repair_ledger
        .as_object_mut()
        .expect("checkpoint object")
        .remove("repair_attempts");
    let missing_repair_ledger = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &missing_repair_ledger,
    );
    assert!(
        missing_repair_ledger
            .expect_err("missing repair ledger must fail")
            .contains("invalid approval checkpoint")
    );
}

#[test]
fn approval_checkpoint_roundtrips_after_visible_batch_approval_rejection() {
    let mut mixed_batch = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    mixed_batch.tool_calls.push(plan_tool_call(
        "plan_1",
        serde_json::json!([{"step": "edit the file", "status": "in_progress"}]),
    ));
    mixed_batch.tool_calls.push(tool_call(
        "batched_edit",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut single_edit = ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    single_edit.tool_calls.push(tool_call(
        "pending_edit",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));

    let result = agent_loop_with_plan_capabilities(
        vec![mixed_batch, single_edit],
        PolicyEngine::new(PermissionProfile::workspace_write()),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract {
            supports_parallel_tool_calls: true,
            ..ProviderProtocolContract::default()
        },
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "edit the file"));

    assert_eq!(result.status, AgentStatus::Blocked);
    assert!(
        result.tool_results.iter().any(|tool_result| {
            tool_result.tool_call_id == "batched_edit"
                && tool_result.failure_kind == Some(ToolFailureKind::Approval)
                && tool_result.error_code.as_deref() == Some("approval_required")
        }),
        "{:?}",
        result.tool_results
    );
    let pending = pending_approval(&result);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
        .expect("visible batch rejection must remain resumable");
}

#[test]
fn recovered_batch_sibling_does_not_invalidate_later_approval_checkpoint() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.md"), "before").expect("fixture");
    let mut mixed_batch = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    mixed_batch.tool_calls.push(tool_call(
        "list_sibling",
        "list",
        serde_json::json!({"path": "."}),
    ));
    mixed_batch.tool_calls.push(tool_call(
        "invalid_command",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "timeout_seconds": 5,
            "unexpected": true
        }),
    ));
    let mut corrected_command =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    corrected_command.tool_calls.push(tool_call(
        "corrected_command",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "timeout_seconds": 5
        }),
    ));
    let mut pending_edit = ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    pending_edit.tool_calls.push(tool_call(
        "pending_edit",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));

    let mut registry = ToolRegistry::default();
    for entry in workspace_tool_entries().into_iter().filter(|entry| {
        ["read", "list", "edit", "patch", "command"].contains(&entry.spec.name.as_str())
    }) {
        registry.register(entry).expect("register workspace tool");
    }
    for entry in agent_control_tool_entries() {
        registry
            .register(entry)
            .expect("register agent control tool");
    }
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![mixed_batch, corrected_command, pending_edit],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract {
                supports_parallel_tool_calls: true,
                ..ProviderProtocolContract::default()
            },
        },
        ToolBroker::new(registry),
        PolicyEngine::new(PermissionProfile::workspace_write()),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new("thread_1", "turn_1", "edit the file after inspection")
            .with_max_turns(3),
    );

    assert_eq!(result.status, AgentStatus::Blocked, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    let sibling = result
        .tool_results
        .iter()
        .find(|tool_result| tool_result.tool_call_id == "list_sibling")
        .expect("batch sibling result");
    assert_eq!(sibling.error_code.as_deref(), Some("tool_batch_rejected"));
    assert_eq!(sibling.failure_kind, Some(ToolFailureKind::Visibility));
    let pending = pending_approval(&result);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
        .expect("recovered batch sibling must not poison approval checkpoint");
}

#[test]
fn pending_approval_occurrences_keep_request_tool_checkpoint_order() {
    let response = |call_id: &str, path: &str| {
        let mut response = ModelTurnResponse::completed(
            "model_request_turn_1_0",
            format!("response_{call_id}"),
            "before approval",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "edit",
            serde_json::json!({
                "path": path,
                "expected": "before",
                "replacement": "after"
            }),
        ));
        response
    };
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit files");
    let first = agent_loop_with_response(
        response("call_1", "first.txt"),
        allow_read_policy().with_rule(
            PermissionRule::new(
                "ask_first",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource(workspace_resource("first.txt")),
        ),
    )
    .run(&input);
    assert_eq!(first.status, AgentStatus::Blocked);
    let second = agent_loop_with_response(
        response("call_2", "second.txt"),
        allow_read_policy().with_rule(
            PermissionRule::new(
                "ask_second",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Write)
            .for_resource(workspace_resource("second.txt")),
        ),
    )
    .run(&input);
    assert_eq!(second.status, AgentStatus::Blocked);
    let first_occurrence = pending_approval(&first);
    let second_occurrence = pending_approval(&second);

    let mut ordered = first.clone();
    ordered.pending_approvals = vec![first_occurrence.clone(), second_occurrence.clone()];
    assert_eq!(
        ordered
            .pending_approvals
            .iter()
            .map(|occurrence| occurrence.request().request_id.as_str())
            .collect::<Vec<_>>(),
        ["approval_turn_1_call_1", "approval_turn_1_call_2"]
    );
    assert_eq!(
        ordered.pending_approvals[0]
            .pending_tool_call()
            .tool_call_id,
        "call_1"
    );
    assert_eq!(
        ordered.pending_approvals[1]
            .pending_tool_call()
            .tool_call_id,
        "call_2"
    );
    let public = serde_json::to_value(&ordered).expect("serialize ordered approvals");
    assert_eq!(
        public["approval_requests"]
            .as_array()
            .expect("approval request projection")
            .iter()
            .map(|request| request["request_id"].as_str().expect("request id"))
            .collect::<Vec<_>>(),
        ["approval_turn_1_call_1", "approval_turn_1_call_2"]
    );

    let mismatched = PendingApprovalOccurrence::from_checkpoint_payload(
        first_occurrence.request().clone(),
        &second_occurrence
            .encode_checkpoint()
            .expect("second checkpoint"),
    );
    assert!(
        mismatched
            .expect_err("mismatched occurrence must fail closed")
            .contains("request mismatch")
    );
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"));
    let blocked = agent_loop.run(&input);
    let pending = pending_approval(&blocked);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    let resume_input = input.clone().with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));

    let restored =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
            .expect("decode current checkpoint");
    let mut legacy_checkpoint = checkpoint.clone();
    legacy_checkpoint["checkpoint_version"] = serde_json::json!(1);
    assert_eq!(
        PendingApprovalOccurrence::from_checkpoint_payload(
            pending.request().clone(),
            &legacy_checkpoint,
        )
        .expect_err("legacy checkpoint must fail closed"),
        "unsupported approval checkpoint version"
    );
    let resumed = agent_loop.resume_pending_approval(&resume_input, &restored);

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
fn approval_pause_resume_matches_uninterrupted_history_and_result_order() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.md"), "stable").expect("write fixture");
    let ask_readme = PermissionRule::new(
        "ask_readme",
        SettingsScope::Project,
        PermissionDecisionOutcome::Ask,
    )
    .for_operation(PermissionOperation::Read)
    .for_resource(workspace_resource("README.md"));
    let policy = allow_read_policy().with_rule(ask_readme);
    let input = AgentLoopInput::new("thread_1", "turn_1", "read the file").with_max_turns(2);
    let grant = ApprovalGrant::allow(
        "approval_turn_1_call_1",
        tool_id("read"),
        [workspace_resource("README.md")],
    );
    let read_response = || {
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
        response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
            attempt_count: 1,
            occurrences: vec![provider_attempt_occurrence(
                1,
                "provider-approval-read",
                ProviderAttemptStatus::Ok,
            )],
            ..Default::default()
        });
        response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
            cache_observations: vec![ProviderCapabilityCacheObservation {
                api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                outcome: ProviderCapabilityCacheLookupResult::Miss,
                observed_at_unix_ms: 4,
                model_turn_ordinal: None,
                parent_occurrence_id: None,
            }],
            ..negotiated_capability_metadata()
        });
        response
    };
    let final_response = || {
        let mut response =
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
        response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
            attempt_count: 1,
            occurrences: vec![provider_attempt_occurrence(
                2,
                "provider-approval-final",
                ProviderAttemptStatus::Ok,
            )],
            ..Default::default()
        });
        response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
            cache_observations: vec![ProviderCapabilityCacheObservation {
                api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                outcome: ProviderCapabilityCacheLookupResult::Hit,
                observed_at_unix_ms: 5,
                model_turn_ordinal: None,
                parent_occurrence_id: None,
            }],
            ..negotiated_capability_metadata()
        });
        response
    };

    let uninterrupted_requests = Arc::new(Mutex::new(Vec::new()));
    let uninterrupted = AgentLoop::new(
        StaticProvider {
            responses: vec![read_response(), final_response()],
            seen_requests: Arc::clone(&uninterrupted_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_policy(),
    )
    .with_workspace_tools(WorkspaceTools::new(workspace.path()).expect("bind workspace tools"));
    let uninterrupted_result = uninterrupted.run(&input.clone().with_approval_grant(grant.clone()));
    assert_eq!(uninterrupted_result.status, AgentStatus::Completed);

    let paused_requests = Arc::new(Mutex::new(Vec::new()));
    let paused = AgentLoop::new(
        StaticProvider {
            responses: vec![read_response(), final_response()],
            seen_requests: Arc::clone(&paused_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        policy.clone(),
    )
    .with_workspace_tools(WorkspaceTools::new(workspace.path()).expect("bind workspace tools"));
    let blocked = paused.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(
        blocked
            .provider_capability_metadata
            .as_ref()
            .expect("blocked capability observations")
            .cache_observations
            .len(),
        1
    );
    let pending = pending_approval(&blocked);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    assert!(checkpoint.get("cache_observations").is_none());
    assert!(checkpoint.get("provider_capability_metadata").is_none());
    let resumed_grant = ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    );

    let resumed_requests = Arc::new(Mutex::new(Vec::new()));
    let resumed_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![final_response()],
            seen_requests: Arc::clone(&resumed_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        policy,
    )
    .with_workspace_tools(WorkspaceTools::new(workspace.path()).expect("bind workspace tools"));
    let resumed =
        resumed_loop.resume_pending_approval(&input.with_approval_grant(resumed_grant), &pending);

    assert_eq!(resumed.status, uninterrupted_result.status);
    assert_eq!(resumed.model_turns, uninterrupted_result.model_turns);
    assert_eq!(resumed.verification, uninterrupted_result.verification);
    assert_eq!(resumed.tool_results, uninterrupted_result.tool_results);
    assert_eq!(
        resumed
            .provider_capability_metadata
            .as_ref()
            .expect("resumed capability observations")
            .cache_observations
            .iter()
            .map(|observation| observation.outcome)
            .collect::<Vec<_>>(),
        [ProviderCapabilityCacheLookupResult::Hit]
    );
    assert_eq!(
        uninterrupted_result
            .provider_capability_metadata
            .as_ref()
            .expect("uninterrupted capability observations")
            .cache_observations
            .iter()
            .map(|observation| observation.outcome)
            .collect::<Vec<_>>(),
        [
            ProviderCapabilityCacheLookupResult::Miss,
            ProviderCapabilityCacheLookupResult::Hit
        ]
    );
    let uninterrupted_requests = uninterrupted_requests.lock().expect("requests");
    let resumed_requests = resumed_requests.lock().expect("requests");
    assert_eq!(uninterrupted_requests.len(), 2);
    assert_eq!(resumed_requests.len(), 1);
    assert_eq!(
        uninterrupted_requests[1].messages,
        resumed_requests[0].messages
    );
    assert_eq!(
        uninterrupted_result
            .tool_results
            .iter()
            .map(|result| (&result.tool_call_id, &result.tool_name))
            .collect::<Vec<_>>(),
        resumed
            .tool_results
            .iter()
            .map(|result| (&result.tool_call_id, &result.tool_name))
            .collect::<Vec<_>>()
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"));

    let blocked = agent_loop.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    let pending = pending_approval(&blocked);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    assert_eq!(
        checkpoint["used_approval_grants"],
        serde_json::json!(["approval_turn_1_call_1"])
    );
    let resume_input = input
        .with_approval_grant(second_grant)
        .with_approval_grant(first_grant);

    let resumed = agent_loop.resume_pending_approval(&resume_input, &pending);

    assert_eq!(resumed.status, AgentStatus::Failed);
    assert_eq!(resumed.model_turns, 3);
    assert_eq!(resumed.approval_count, 1);
    assert!(resumed.pending_approvals.is_empty());
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
    let pending = pending_approval(&blocked);
    let mut checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    checkpoint["completion"]["workspace_mutated"] = serde_json::json!(true);
    let resumed =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint);

    assert_eq!(
        resumed.expect_err("tampered completion checkpoint must fail"),
        "approval checkpoint workspace revision state is invalid"
    );
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
        .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    let required_argv = test_command("second-success");
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
    let mut events = Vec::new();

    let result = agent_loop_with_capabilities(
        vec![command_response, required_verification, final_response],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        capabilities,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(LargeOutputBackend),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "run the command")
            .with_max_turns(3)
            .with_verification_commands([verification_command(required_argv.join(" "), 1)]),
        &mut |event| {
            events.push(event);
            Ok(())
        },
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
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AgentLoopEvent::Observation(AgentObservation::PromptAssembly(value))
                    if value.compacted
                        && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                            status: PromptAssemblyStatus::Ready,
                            ..
                        })
            ))
            .count(),
        2
    );
}

#[test]
fn approval_resume_finishes_the_same_tool_occurrence_identity() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "approved read").expect("write fixture");
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    tool_response.tool_calls.push(tool_call(
        "approval_call",
        "read",
        serde_json::json!({
            "path": "README.md",
            "max_chars": null,
            "line_start": null,
            "line_end": null
        }),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = agent_loop_with_responses_and_requests(
        vec![
            tool_response,
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
        ],
        PolicyEngine::new(PermissionProfile::workspace_write()),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"));
    let input = AgentLoopInput::new("thread_1", "turn_1", "read after approval").with_max_turns(2);
    let mut initial_events = Vec::new();

    let blocked = agent_loop.run_with_events(&input, &mut |event| {
        initial_events.push(event);
        Ok(())
    });
    assert_eq!(blocked.status, AgentStatus::Blocked);
    let pending = pending_approval(&blocked);
    let suspended = initial_events
        .iter()
        .find_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
                if matches!(value.lifecycle, OccurrenceLifecycle::Suspended { .. }) =>
            {
                Some(value.identity.occurrence_id.clone())
            }
            _ => None,
        })
        .expect("suspended tool occurrence");
    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let mut resumed_events = Vec::new();

    let resumed =
        agent_loop.resume_pending_approval_with_events(&resumed_input, &pending, &mut |event| {
            resumed_events.push(event);
            Ok(())
        });

    assert_eq!(
        resumed.status,
        AgentStatus::Completed,
        "error={:?} tool_results={:?}",
        resumed.error,
        resumed.tool_results
    );
    let finished = resumed_events
        .iter()
        .find_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
                if matches!(
                    value.lifecycle,
                    OccurrenceLifecycle::Finished {
                        status: ToolCallStatus::Succeeded,
                        ..
                    }
                ) =>
            {
                Some(value.identity.occurrence_id.clone())
            }
            _ => None,
        })
        .expect("finished resumed tool occurrence");
    assert_eq!(finished, suspended);
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 2);
}

#[test]
fn agent_loop_pairs_duplicate_tool_call_ids_by_result_occurrence_for_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut first_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first_response.tool_calls.push(tool_call(
        "reused_call",
        "command",
        serde_json::json!({
            "command": test_command_script("small-first"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut second_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    second_response.tool_calls.push(tool_call(
        "reused_call",
        "command",
        serde_json::json!({
            "command": test_command_script("large-second"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_capabilities(
        vec![first_response, second_response, final_response],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract {
            max_context_tokens: 1_500,
            max_output_tokens: 128,
            ..ProviderProtocolContract::default()
        },
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(SequencedOutputBackend::default()),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "run both commands").with_max_turns(3));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(
        result
            .context_trace
            .as_ref()
            .expect("context trace")
            .compaction_count,
        1
    );
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 3);
}

#[test]
fn verification_plan_shares_one_exact_action_across_multiple_risks() {
    let scope = command_script_scope_digest_with_policy(
        &test_command_script("success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let plan = AgentVerificationPlan {
        risks: vec![
            AgentVerificationRisk::EmptyCollection,
            AgentVerificationRisk::ZeroValue,
        ],
        checks: vec![
            AgentVerificationCheck::new(
                AgentVerificationRisk::EmptyCollection,
                AgentVerificationRequirement::new(&scope, 1),
            ),
            AgentVerificationCheck::new(
                AgentVerificationRisk::ZeroValue,
                AgentVerificationRequirement::new(&scope, 1),
            ),
        ],
        entries: Vec::new(),
    };

    plan.validate().expect("shared exact action plan");
    assert_eq!(
        plan.requirements(),
        vec![AgentVerificationRequirement::new(scope, 1)]
    );
}

#[test]
fn verification_plan_rejects_a_check_bound_to_a_different_entry_action() {
    let action = AgentVerificationAction {
        command: test_command_script("entry-action"),
        cwd: ".".to_string(),
        timeout_seconds: 5,
        sandbox_mode: SandboxFilesystemMode::WorkspaceWrite,
        network_access: SandboxNetworkMode::Denied,
    };
    let different_scope = command_script_scope_digest_with_policy(
        &test_command_script("different-check"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let plan = AgentVerificationPlan {
        risks: vec![AgentVerificationRisk::GeneralMutation],
        checks: vec![AgentVerificationCheck::new(
            AgentVerificationRisk::GeneralMutation,
            AgentVerificationRequirement::new(different_scope, 1),
        )],
        entries: vec![AgentVerificationEntry {
            risk: AgentVerificationRisk::GeneralMutation,
            evidence: "exercise the changed behavior".to_string(),
            affected_path: "README.md".to_string(),
            affected_symbol: "documented_behavior".to_string(),
            current_gap: "the changed behavior is not verified".to_string(),
            action,
        }],
    };

    assert_eq!(
        plan.validate().expect_err("mismatched binding"),
        "verification entry must exactly match its risk and command scope binding"
    );
}

#[test]
fn exact_verification_ignores_wrong_or_pre_mutation_results_and_counts_duplicates() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit and verify")
        .with_max_turns(7)
        .with_verification_commands([verification_command(test_command_script("success"), 2)]);

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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
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

/// 两次 mutation 后模型重新声明同一 digest：完成门禁必须仍按调用方下限要求 2 次成功，
/// 不能把模型单次 requirement 当成降低 typed caller contract，提前的 final answer 被拒。
#[test]
fn caller_verification_floor_survives_replan_with_single_model_requirement() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit twice and verify")
        .with_max_turns(7)
        .with_verification_commands([verification_command(test_command_script("success"), 2)]);
    let shrunk_plan = workspace_verification_plan_response(
        "model_request_turn_1_2",
        "response_2",
        "shrunk_plan",
        "success",
    );
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_edit_response(
                    "model_request_turn_1_0",
                    "response_0",
                    "edit_0",
                    "before",
                    "intermediate",
                ),
                workspace_edit_response(
                    "model_request_turn_1_1",
                    "response_1",
                    "edit_1",
                    "intermediate",
                    "after",
                ),
                shrunk_plan,
                workspace_command_response(
                    "model_request_turn_1_3",
                    "response_3",
                    "command_3",
                    "success",
                ),
                ModelTurnResponse::completed("model_request_turn_1_4", "response_4", "premature"),
                workspace_command_response(
                    "model_request_turn_1_5",
                    "response_5",
                    "command_5",
                    "success",
                ),
                ModelTurnResponse::completed("model_request_turn_1_6", "response_6", "done"),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed, "{result:?}");
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.verification.required_command_count, 2);
    assert_eq!(result.verification.satisfied_command_count, 2);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
}

/// A caller floor may name a different exact command than the model plan. The completion gate
/// must retain both scopes, and the caller-owned action remains available to revision replanning.
#[test]
fn caller_and_model_verification_scopes_use_caller_owned_action_union() {
    let dir = tempfile::tempdir().expect("different scope workspace");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let caller_command = test_command_script("caller-scope");
    let model_command = test_command_script("model-scope");
    let caller_digest = command_script_scope_digest_with_policy(
        &caller_command,
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let model_digest = command_script_scope_digest_with_policy(
        &model_command,
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let incomplete_plan = workspace_verification_plan_response_with_commands(
        "model_request_turn_different_scope_2",
        "response_different_scope_2",
        "plan_different_scope",
        &["model-scope"],
    );
    let complete_plan = workspace_verification_plan_response_with_commands(
        "model_request_turn_different_scope_3",
        "response_different_scope_3",
        "plan_different_scope_complete",
        &["model-scope", "caller-scope"],
    );
    let mut model_verification = ModelTurnResponse::completed(
        "model_request_turn_different_scope_4",
        "response_different_scope_4",
        "",
    );
    model_verification.tool_calls.push(tool_call(
        "command_model_scope",
        "command",
        serde_json::json!({"command": model_command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut caller_verification = ModelTurnResponse::completed(
        "model_request_turn_different_scope_5",
        "response_different_scope_5",
        "",
    );
    caller_verification.tool_calls.push(tool_call(
        "command_caller_scope",
        "command",
        serde_json::json!({"command": caller_command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_edit_response(
                    "model_request_turn_different_scope_0",
                    "response_different_scope_0",
                    "edit_different_scope",
                    "before",
                    "middle",
                ),
                workspace_edit_response(
                    "model_request_turn_different_scope_1",
                    "response_different_scope_1",
                    "edit_different_scope_again",
                    "middle",
                    "after",
                ),
                incomplete_plan,
                complete_plan,
                model_verification,
                caller_verification,
                ModelTurnResponse::completed(
                    "model_request_turn_different_scope_6",
                    "response_different_scope_6",
                    "done",
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind different scope workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_different_scope",
            "turn_different_scope",
            "edit and run both verification scopes",
        )
        .with_max_turns(7)
        .with_verification_commands([verification_command(caller_command.clone(), 1)]),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.verification.required_command_count, 2);
    assert_eq!(result.verification.satisfied_command_count, 2);
    let rejected_plan = result
        .tool_results
        .iter()
        .find(|result| result.tool_call_id == "plan_different_scope")
        .expect("incomplete plan result");
    assert_eq!(
        rejected_plan.error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    let rejected_payload = rejected_plan.to_message_payload();
    let rejected_summary = rejected_payload["content"]["summary"]
        .as_str()
        .expect("safe incomplete-plan summary");
    assert!(rejected_summary.contains("all caller-required exact checks"));
    assert!(!rejected_summary.contains(&caller_command));
    let command_digests = result
        .tool_results
        .iter()
        .filter(|result| result.tool_name == "command" && result.ok)
        .filter_map(|result| {
            result
                .audit_metadata()
                .and_then(|metadata| metadata.get("command_scope_digest"))
                .and_then(serde_json::Value::as_str)
        })
        .collect::<Vec<_>>();
    assert_eq!(command_digests.len(), 2);
    assert!(command_digests.contains(&caller_digest.as_str()));
    assert!(command_digests.contains(&model_digest.as_str()));
    let requests = seen_requests.lock().expect("different scope requests");
    assert!(requests[2].tools.iter().all(|tool| tool.name != "command"));
    assert!(
        requests[2]
            .tools
            .iter()
            .any(|tool| tool.name == "update_plan")
    );
    assert!(requests[3].tools.iter().all(|tool| tool.name != "command"));
    assert!(
        requests[3]
            .tools
            .iter()
            .any(|tool| tool.name == "update_plan")
    );
    let model_command_schema = &requests[4]
        .tools
        .iter()
        .find(|tool| tool.name == "command")
        .expect("model exact command tool")
        .parameters_schema;
    assert_eq!(
        model_command_schema["properties"]["command"]["const"],
        serde_json::json!(model_command)
    );
    let caller_command_schema = &requests[5]
        .tools
        .iter()
        .find(|tool| tool.name == "command")
        .expect("caller exact command tool")
        .parameters_schema;
    assert_eq!(
        caller_command_schema["properties"]["command"]["const"],
        serde_json::json!(caller_command)
    );
    assert!(requests[1].messages.iter().any(|message| {
        message.role == ModelRole::Developer
            && message.content.contains("caller_verification_commands=")
            && message.content.contains(&caller_command)
    }));
}

/// A model plan may lower a repeated count for a scope, but the caller floor remains authoritative
/// when selecting the next exact command after the first successful observation.
#[test]
fn caller_floor_keeps_shared_scope_pinned_after_plan_count_shrink() {
    let dir = tempfile::tempdir().expect("shared scope workspace");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let command = test_command_script("shared-scope");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_edit_response(
                    "model_request_turn_shared_scope_0",
                    "response_shared_scope_0",
                    "edit_shared_scope_0",
                    "before",
                    "middle",
                ),
                workspace_edit_response(
                    "model_request_turn_shared_scope_1",
                    "response_shared_scope_1",
                    "edit_shared_scope_1",
                    "middle",
                    "after",
                ),
                workspace_verification_plan_response(
                    "model_request_turn_shared_scope_2",
                    "response_shared_scope_2",
                    "plan_shared_scope",
                    "shared-scope",
                ),
                workspace_command_response(
                    "model_request_turn_shared_scope_3",
                    "response_shared_scope_3",
                    "command_shared_scope_0",
                    "shared-scope",
                ),
                ModelTurnResponse::completed(
                    "model_request_turn_shared_scope_4",
                    "response_shared_scope_4",
                    "not finished",
                ),
                workspace_command_response(
                    "model_request_turn_shared_scope_5",
                    "response_shared_scope_5",
                    "command_shared_scope_1",
                    "shared-scope",
                ),
                ModelTurnResponse::completed(
                    "model_request_turn_shared_scope_6",
                    "response_shared_scope_6",
                    "done",
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind shared scope workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_shared_scope",
            "turn_shared_scope",
            "edit and run the shared verification twice",
        )
        .with_max_turns(7)
        .with_verification_commands([verification_command(command.clone(), 2)]),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.verification.required_command_count, 2);
    assert_eq!(result.verification.satisfied_command_count, 2);
    let requests = seen_requests.lock().expect("shared scope requests");
    let pinned = &requests[4];
    assert_eq!(pinned.tools.len(), 1, "request={pinned:?}");
    assert_eq!(pinned.tools[0].name, "command");
    assert_eq!(
        pinned.tools[0].parameters_schema["properties"]["command"]["const"],
        serde_json::json!(command)
    );
}

/// TurnCheckpoint 必须持久化并恢复覆盖 caller floor 的完整模型 plan，resume 后完成门禁
/// 仍要求调用方与模型 plan 的合并 command 集。
#[test]
fn caller_verification_floor_survives_turn_checkpoint_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let caller_digest = command_script_scope_digest_with_policy(
        &test_command_script("success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let plan_digest = command_script_scope_digest_with_policy(
        &test_command_script("second-success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit twice and verify")
        .with_max_turns(3)
        .with_verification_commands([verification_command(test_command_script("success"), 1)]);
    let complete_plan = workspace_verification_plan_response_with_commands(
        "model_request_turn_1_2",
        "response_2",
        "complete_plan",
        &["second-success", "success"],
    );
    let policy = || {
        allow_read_execute_policy().with_rule(
            PermissionRule::new(
                "allow_write",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write),
        )
    };
    let mut checkpoints = Vec::new();

    let interrupted = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_edit_response(
                    "model_request_turn_1_0",
                    "response_0",
                    "edit_0",
                    "before",
                    "intermediate",
                ),
                workspace_edit_response(
                    "model_request_turn_1_1",
                    "response_1",
                    "edit_1",
                    "intermediate",
                    "after",
                ),
                complete_plan,
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_events_and_checkpoints(&input, &mut |_event| Ok(()), &mut |checkpoint| {
        checkpoints.push(checkpoint);
        Ok(())
    });

    assert_eq!(interrupted.status, AgentStatus::Failed, "{interrupted:?}");
    let checkpoint = checkpoints
        .iter()
        .rfind(|event| {
            event
                .checkpoint
                .encode()
                .is_ok_and(|payload| !payload["verification_plan"].is_null())
        })
        .expect("checkpoint with installed verification plan")
        .checkpoint
        .clone();
    let payload = checkpoint.encode().expect("encode checkpoint");
    assert_eq!(
        payload["completion"]["required_command_counts"],
        serde_json::json!({ caller_digest.clone(): 1, plan_digest.clone(): 1 }),
        "checkpoint must persist the caller requirement unioned with the model plan"
    );

    let resumed = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_command_response(
                    "model_request_turn_1_3",
                    "response_3",
                    "command_3",
                    "second-success",
                ),
                workspace_command_response(
                    "model_request_turn_1_4",
                    "response_4",
                    "command_4",
                    "success",
                ),
                ModelTurnResponse::completed("model_request_turn_1_5", "response_5", "done"),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .resume_turn(&input.with_max_turns(8), &checkpoint);

    assert_eq!(resumed.status, AgentStatus::Completed, "{resumed:?}");
    assert_eq!(resumed.final_answer.as_deref(), Some("done"));
    assert_eq!(resumed.verification.required_command_count, 2);
    assert_eq!(resumed.verification.satisfied_command_count, 2);
}

/// ApprovalCheckpoint 必须持久化合并要求集：批准后 resume 的完成门禁仍要求调用方
/// command 与模型 plan command 都成功。
#[test]
fn caller_verification_floor_survives_approval_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let caller_digest = command_script_scope_digest_with_policy(
        &test_command_script("success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let plan_digest = command_script_scope_digest_with_policy(
        &test_command_script("second-success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit twice and verify")
        .with_max_turns(6)
        .with_verification_commands([verification_command(test_command_script("success"), 1)]);
    let complete_plan = workspace_verification_plan_response_with_commands(
        "model_request_turn_1_2",
        "response_2",
        "complete_plan",
        &["second-success", "success"],
    );
    let policy = allow_read_write_policy().with_rule(
        PermissionRule::new(
            "ask_execute",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
        )
        .for_operation(PermissionOperation::Execute),
    );

    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                workspace_edit_response(
                    "model_request_turn_1_0",
                    "response_0",
                    "edit_0",
                    "before",
                    "intermediate",
                ),
                workspace_edit_response(
                    "model_request_turn_1_1",
                    "response_1",
                    "edit_1",
                    "intermediate",
                    "after",
                ),
                complete_plan,
                workspace_command_response(
                    "model_request_turn_1_3",
                    "response_3",
                    "command_3",
                    "second-success",
                ),
                workspace_command_response(
                    "model_request_turn_1_4",
                    "response_4",
                    "command_4",
                    "success",
                ),
                ModelTurnResponse::completed("model_request_turn_1_5", "response_5", "done"),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked, "{blocked:?}");
    let pending = pending_approval(&blocked);
    let payload = pending
        .encode_checkpoint()
        .expect("encode approval checkpoint");
    assert_eq!(
        payload["completion"]["required_command_counts"],
        serde_json::json!({ caller_digest.clone(): 1, plan_digest.clone(): 1 }),
        "approval checkpoint must persist the caller requirement unioned with the model plan"
    );
    let steered = pending
        .checkpoint()
        .into_turn_checkpoint(
            &["continue with a different approach".to_string()],
            true,
            &input
                .verification_requirements()
                .expect("caller verification requirements"),
        )
        .expect("steer after installed model plan");
    let steered_payload = steered.encode().expect("encode steered checkpoint");
    assert!(
        steered_payload["verification_plan"].is_null(),
        "steer must clear an installed model plan"
    );
    assert_eq!(
        steered_payload["completion"]["required_command_counts"],
        serde_json::json!({ caller_digest.clone(): 1 }),
        "steer must restore only the explicit caller floor"
    );

    let resumed_input = input.clone().with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let blocked_again = agent_loop.resume_pending_approval(&resumed_input, &pending);

    assert_eq!(
        blocked_again.status,
        AgentStatus::Blocked,
        "{blocked_again:?}"
    );
    let pending_again = pending_approval(&blocked_again);
    let resumed_input_again = input.clone().with_approval_grant(ApprovalGrant::allow(
        pending_again.pending_tool_call().request_id.clone(),
        pending_again.pending_tool_call().tool_name.clone(),
        pending_again.pending_tool_call().resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_approval(&resumed_input_again, &pending_again);

    assert_eq!(resumed.status, AgentStatus::Completed, "{resumed:?}");
    assert_eq!(resumed.final_answer.as_deref(), Some("done"));
    assert_eq!(resumed.verification.required_command_count, 2);
    assert_eq!(resumed.verification.satisfied_command_count, 2);
}

/// approval steer 即使发生在模型 plan 安装前，也必须从显式 caller requirements 恢复
/// 完成门禁下限；恢复过程不能依赖 synthetic plan 或 checkpoint 形状推断。
#[test]
fn caller_verification_floor_survives_approval_steer() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let caller_digest = command_script_scope_digest_with_policy(
        &test_command_script("success"),
        ".",
        5,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit and verify")
        .with_max_turns(4)
        .with_verification_commands([verification_command(test_command_script("success"), 1)]);
    let edit = workspace_edit_response(
        "model_request_turn_1_0",
        "response_0",
        "edit_0",
        "before",
        "after",
    );
    let mut steps_only_plan =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_1", "");
    steps_only_plan.tool_calls.push(tool_call(
        "steps_only_plan",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the workspace state", "status": "completed"}]
        }),
    ));
    let command = workspace_command_response(
        "model_request_turn_1_2",
        "response_2",
        "command_2",
        "success",
    );
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "ask_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Ask,
        )
        .for_operation(PermissionOperation::Write),
    );

    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit,
                steps_only_plan,
                command,
                ModelTurnResponse::completed("model_request_turn_1_3", "response_3", "done"),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked, "{blocked:?}");
    let pending = pending_approval(&blocked);
    let steered = pending
        .checkpoint()
        .into_turn_checkpoint(
            &["verify without editing".to_string()],
            true,
            &input
                .verification_requirements()
                .expect("caller verification requirements"),
        )
        .expect("approval steer handoff");
    let payload = steered.encode().expect("encode steered checkpoint");
    assert!(
        payload["verification_plan"].is_null(),
        "steer must discard the old model plan"
    );
    assert_eq!(
        payload["completion"]["required_command_counts"],
        serde_json::json!({ caller_digest.clone(): 1 }),
        "steer must keep the caller-declared requirement set"
    );

    let resumed = agent_loop.resume_turn(&input, &steered);

    assert_eq!(resumed.status, AgentStatus::Completed, "{resumed:?}");
    assert_eq!(resumed.final_answer.as_deref(), Some("done"));
    assert_eq!(resumed.verification.required_command_count, 1);
    assert_eq!(resumed.verification.satisfied_command_count, 1);
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
    let mut events = Vec::new();

    let result = agent_loop_with_responses_and_requests(
        vec![denied, allowed, final_response],
        policy,
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(workspace.path()).expect("bind workspace tools"))
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "read if allowed").with_max_turns(3),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

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
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    let denied_feedback = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == ModelRole::Tool && message.tool_call_id.as_deref() == Some("denied")
        })
        .expect("denied tool feedback");
    let denied_call = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.tool_calls.iter())
        .find(|call| call.tool_call_id == "denied")
        .expect("denied assistant tool call");
    assert_eq!(
        denied_call.arguments,
        serde_json::json!({"path": "README.md"}),
        "a policy denial must preserve already-validated provider history"
    );
    assert!(
        denied_feedback
            .content
            .contains("\"failure_kind\":\"policy\"")
    );
    assert!(!denied_feedback.content.contains("rejection_kind"));
    assert!(!denied_feedback.content.contains("placeholder_non_callable"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("README.md")).expect("fixture remains"),
        "unchanged"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::PolicyDecision(value))
            if value.cause == Some(PolicyDecisionCause::Rule)
                && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: PolicyDecisionStatus::Deny,
                    ..
                })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
            if matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                status: ToolCallStatus::PolicyDenied,
                ..
            })
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::SandboxExecution(_))
    )));
}

#[test]
fn approval_resume_preserves_exact_verification_and_compaction_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("README.md"), "before").expect("write file");
    let sandbox_mode = SandboxFilesystemMode::WorkspaceWrite;
    let network_access = SandboxNetworkMode::Denied;
    let first_argv = test_command("success");
    let second_argv = test_command("second-success");
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit and verify twice")
        .with_max_turns(3)
        .with_verification_commands([
            verification_command(first_argv.join(" "), 1),
            verification_command(second_argv.join(" "), 1),
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
    let continuation_final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
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
    let continuation_capabilities = capabilities.clone();
    let continuation_policy = policy.clone();
    let agent_loop = agent_loop_with_capabilities(
        vec![edit, first_verification, pending_verification],
        policy,
        Arc::new(Mutex::new(Vec::new())),
        capabilities,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(LargeOutputBackend),
    );

    let blocked = agent_loop.run(&input);
    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(blocked.verification.required_command_count, 2);
    assert_eq!(blocked.verification.satisfied_command_count, 1);
    let pending = pending_approval(&blocked);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    assert_eq!(checkpoint["context_trace"]["compaction_count"], 1);
    assert_eq!(
        checkpoint["completion"]["terminal_command_scope_digests"]
            .as_array()
            .expect("terminal command observations")
            .len(),
        1
    );
    assert_eq!(checkpoint["completion"]["workspace_revision"], 1);
    assert_eq!(
        checkpoint["completion"]["terminal_command_revisions"],
        serde_json::json!([1])
    );
    let checkpoint_command = checkpoint["tool_result_occurrences"]
        .as_array()
        .expect("checkpoint tool results")
        .iter()
        .find(|result| result["result"]["tool_name"] == "command")
        .expect("checkpoint command result");
    assert_eq!(
        checkpoint_command["workspace_observation"],
        serde_json::json!({"revision": 1, "mutation": "unchanged"})
    );

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let resumed_agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![continuation_final_response],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: continuation_capabilities,
        },
        agent_tool_broker_for_test(false),
        continuation_policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind resumed workspace tools")
            .with_sandbox_backend(LargeOutputBackend),
    );

    let mut missing_context_count = checkpoint.clone();
    missing_context_count["tool_result_occurrences"]
        .as_array_mut()
        .expect("checkpoint tool results")
        .iter_mut()
        .find(|result| result["result"]["tool_name"] == "command")
        .expect("checkpoint command result")
        .as_object_mut()
        .expect("checkpoint command object")
        .remove("context_token_count");
    let missing = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &missing_context_count,
    );
    assert!(
        missing
            .expect_err("missing accounting must fail")
            .contains("context token")
    );

    let mut low_context_count = checkpoint.clone();
    low_context_count["tool_result_occurrences"]
        .as_array_mut()
        .expect("checkpoint tool results")
        .iter_mut()
        .find(|result| result["result"]["tool_name"] == "command")
        .expect("checkpoint command result")["context_token_count"] = serde_json::json!(1);
    let low = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &low_context_count,
    );
    assert!(
        low.expect_err("inconsistent accounting must fail")
            .contains("context token")
    );

    let command_result_index = checkpoint["tool_result_occurrences"]
        .as_array()
        .expect("checkpoint tool results")
        .iter()
        .position(|result| result["result"]["tool_name"] == "command")
        .expect("checkpoint command result");
    let mut compacted_binding = checkpoint.clone();
    compacted_binding["tool_result_occurrences"][command_result_index]["visibility"] =
        serde_json::json!("visible");
    let compacted_binding_result = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &compacted_binding,
    )
    .expect("decode visibility binding");
    let compacted_binding_result =
        resumed_agent_loop.resume_pending_approval(&resumed_input, &compacted_binding_result);
    assert!(
        compacted_binding_result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("tool result occurrence bindings"))
    );

    let mut visible_approval_with_count = checkpoint.clone();
    visible_approval_with_count["tool_result_occurrences"][command_result_index]["result"]["failure_kind"] =
        serde_json::json!("approval");
    let visible_approval_with_count_result = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &visible_approval_with_count,
    );
    assert!(
        visible_approval_with_count_result
            .expect_err("visible approval binding must fail")
            .contains("hidden tool result binding")
    );

    let mut visible_approval_missing_count = checkpoint.clone();
    visible_approval_missing_count["tool_result_occurrences"][command_result_index]["result"]["failure_kind"] =
        serde_json::json!("approval");
    visible_approval_missing_count["tool_result_occurrences"][command_result_index]
        .as_object_mut()
        .expect("checkpoint command object")
        .remove("context_token_count");
    let visible_approval = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &visible_approval_missing_count,
    );
    assert!(
        visible_approval
            .expect_err("hidden approval accounting must fail")
            .contains("hidden tool result binding")
    );

    let mut legacy_checkpoint = checkpoint.clone();
    legacy_checkpoint["checkpoint_version"] = serde_json::json!(1);
    let legacy = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &legacy_checkpoint,
    );
    assert_eq!(
        legacy.expect_err("legacy checkpoint must fail closed"),
        "unsupported approval checkpoint version"
    );
    let restored =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
            .expect("decode current checkpoint");
    let resumed = resumed_agent_loop.resume_pending_approval(&resumed_input, &restored);

    assert_eq!(
        resumed.status,
        AgentStatus::Completed,
        "error={:?} tool_results={:?} verification={:?}",
        resumed.error,
        resumed.tool_results,
        resumed.verification
    );
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
    let public = serde_json::to_string(&resumed).expect("serialize public result");
    assert!(!public.contains("workspace_revision"));
    assert!(!public.contains("workspace_observation"));
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&input);

    assert!(result.error.is_none(), "error={:?}", result.error);
    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.approval_count, 0);
    assert_eq!(result.model_turns, 3);
    assert!(result.pending_approvals.is_empty());
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
    let input = AgentLoopInput::new("thread_1", "turn_1", "hello").with_max_turns(5);
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
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let mut changed_tool_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    changed_tool_response.tool_calls.push(tool_call(
        "call_3",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut verification_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "");
    verification_response.tool_calls.push(tool_call(
        "call_4",
        "command",
        serde_json::json!({"command": "cargo test", "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_4", "response_5", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write())
        .with_rule(
            PermissionRule::new(
                "allow_read",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Read),
        )
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
            changed_tool_response,
            verification_response,
            final_response,
        ],
        policy,
        seen_requests.clone(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 5);
    assert_eq!(result.tool_results.len(), 4);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("expected_content_missing")
    );
    assert!(result.tool_results[1].ok);
    assert!(result.tool_results[2].ok);
    assert!(result.tool_results[3].ok);
    assert!(result.verification.passed);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "after"
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].tool_choice.mode, ToolChoiceMode::Auto);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::Auto);
    let feedback = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool feedback");
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
    let post_diagnostic_feedback = requests[2]
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == ModelRole::Developer && message.content.contains(" repair_context=")
        })
        .expect("post-diagnostic repair context");
    let context: serde_json::Value = serde_json::from_str(
        post_diagnostic_feedback
            .content
            .split_once(" repair_context=")
            .expect("repair context delimiter")
            .1,
    )
    .expect("structured repair context");
    assert_eq!(
        context["failed_requirement"],
        "workspace_mutation:expected_content_missing"
    );
    assert!(
        context["evidence"]
            .as_str()
            .is_some_and(|evidence| evidence.contains("expected content not found"))
    );
    assert_eq!(context["previous_action"], "read");
    assert!(
        context["previous_result"]
            .as_str()
            .is_some_and(|result| result.contains("before"))
    );
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
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
    let feedback = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool feedback");
    let rejected_call = requests[1]
        .messages
        .iter()
        .find(|message| {
            message.role == ModelRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_call_id == "call_1")
        })
        .and_then(|message| {
            message
                .tool_calls
                .iter()
                .find(|call| call.tool_call_id == "call_1")
        })
        .expect("rejected command assistant call");
    assert_eq!(rejected_call.arguments, serde_json::json!({}));
    assert_eq!(rejected_call.raw_arguments, "{}");
    assert_eq!(feedback.role, ModelRole::Tool);
    let payload: serde_json::Value =
        serde_json::from_str(&feedback.content).expect("structured tool payload");
    assert_eq!(payload["error_code"], "invalid_tool_arguments");
    assert_eq!(payload["content"]["validation_code"], "command_not_string");
    for field in [
        "visible_tool_names",
        "rejection_kind",
        "name_projection",
        "correction",
        "placeholder_non_callable",
    ] {
        assert!(
            payload["content"].get(field).is_none(),
            "unexpected field {field}"
        );
    }
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
fn agent_loop_projects_invalid_json_tool_calls_with_safe_history_and_feedback() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("README.md"), "ready").expect("fixture");
    let mut malformed = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    malformed.tool_calls.push(ModelToolCall {
        tool_call_id: "parse_1".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({}),
        raw_arguments: r#"{"path":"C:\\secrets\\token"}"#.to_string(),
        parse_status: ModelToolParseStatus::InvalidJson,
        validation_errors: vec!["invalid_json".to_string()],
    });
    let mut repaired = ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    repaired.tool_calls.push(tool_call(
        "read_1",
        "read",
        serde_json::json!({"path": "README.md"}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_responses_and_requests(
        vec![malformed, repaired, final_response],
        allow_read_policy(),
        Arc::clone(&seen_requests),
    )
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("workspace tools"))
    .run(&AgentLoopInput::new("thread_1", "turn_1", "read"));

    assert_eq!(result.status, AgentStatus::Completed, "{result:?}");
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    let requests = seen_requests.lock().expect("seen requests");
    let assistant_positions = requests[1]
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == ModelRole::Assistant
                && message
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_call_id == "parse_1")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let tool_positions = requests[1]
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == ModelRole::Tool && message.tool_call_id.as_deref() == Some("parse_1")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(assistant_positions.len(), 1);
    assert_eq!(tool_positions.len(), 1);
    assert!(
        assistant_positions[0] < tool_positions[0],
        "assistant tool call must precede its paired ToolResult"
    );
    let rejected_call = requests[1].messages[assistant_positions[0]]
        .tool_calls
        .iter()
        .find(|call| call.tool_call_id == "parse_1")
        .expect("parse rejection assistant call");
    assert_eq!(rejected_call.tool_name, "read");
    assert_eq!(rejected_call.arguments, serde_json::json!({}));
    assert_eq!(rejected_call.raw_arguments, "{}");
    let feedback = &requests[1].messages[tool_positions[0]];
    let payload: serde_json::Value =
        serde_json::from_str(&feedback.content).expect("structured feedback");
    assert_eq!(payload["tool_call_id"], "parse_1");
    assert_eq!(payload["tool_name"], "read");
    for field in [
        "visible_tool_names",
        "rejection_kind",
        "name_projection",
        "correction",
        "placeholder_non_callable",
    ] {
        assert!(
            payload["content"].get(field).is_none(),
            "unexpected field {field}"
        );
    }
    assert!(!feedback.content.contains("tool_rejected"));
    assert!(!feedback.content.contains("C:\\secrets\\token"));
    assert!(
        requests[1]
            .tools
            .iter()
            .all(|tool| tool.name != "tool_rejected")
    );
}

#[test]
fn invalid_verification_command_input_does_not_open_a_mutation_bound_repair_cycle() {
    let dir = tempfile::tempdir().expect("temp dir");
    let fixture_name = "invalid_verification_input.txt";
    std::fs::write(dir.path().join(fixture_name), "before").expect("write fixture");
    let command = test_command_script("success");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_1_0",
        "response_invalid_verification_0",
        "",
    );
    edit.tool_calls.push(tool_call(
        "edit_invalid_verification",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_1_1",
        "response_invalid_verification_1",
        "",
    );
    plan.tool_calls.push(tool_call(
        "plan_invalid_verification",
        "update_plan",
        serde_json::json!({
            "steps": [
                {"step": "change the fixture", "status": "completed"},
                {"step": "verify the changed fixture", "status": "in_progress"}
            ],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "the fixture changed",
                "affected_path": fixture_name,
                "affected_symbol": "invalid_verification_input::value",
                "current_gap": "the changed revision is not verified",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut invalid_command = ModelTurnResponse::completed(
        "model_request_turn_1_2",
        "response_invalid_verification_2",
        "",
    );
    invalid_command.tool_calls.push(tool_call(
        "invalid_verification_command",
        "command",
        serde_json::json!({
            "command": command,
            "cwd": ".",
            "timeout_seconds": 5,
            "sandbox_mode": "workspace_write",
            "network_access": "denied"
        }),
    ));
    let mut valid_command = ModelTurnResponse::completed(
        "model_request_turn_1_3",
        "response_invalid_verification_3",
        "",
    );
    valid_command.tool_calls.push(tool_call(
        "valid_verification_command",
        "command",
        serde_json::json!({
            "command": command,
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut completed_plan = ModelTurnResponse::completed(
        "model_request_turn_1_4",
        "response_invalid_verification_4",
        "",
    );
    completed_plan.tool_calls.push(tool_call(
        "complete_invalid_verification_plan",
        "update_plan",
        serde_json::json!({
            "steps": [
                {"step": "change the fixture", "status": "completed"},
                {"step": "verify the changed fixture", "status": "completed"}
            ],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "the fixture changed",
                "affected_path": fixture_name,
                "affected_symbol": "invalid_verification_input::value",
                "current_gap": "the changed revision is not verified",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_1_5",
        "response_invalid_verification_5",
        "done",
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit,
                plan,
                invalid_command,
                valid_command,
                completed_plan,
                final_response,
            ],
            seen_requests,
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_invalid_verification",
            "turn_1",
            "change and verify the fixture",
        )
        .with_max_turns(6),
    );

    assert_eq!(result.status, AgentStatus::Completed, "{result:?}");
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert!(result.verification.passed);
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert_eq!(
        result.tool_results[2].error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    assert_eq!(
        result.tool_results[2]
            .audit_metadata()
            .expect("invalid command audit")["executor_started"],
        false
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join(fixture_name)).expect("read fixture"),
        "after"
    );
}

#[test]
fn verification_plan_precondition_recovery_persists_checkpoint_without_consuming_repair_budget() {
    let workspace = tempfile::tempdir().expect("verification plan recovery workspace");
    std::fs::write(workspace.path().join("README.md"), "before")
        .expect("write verification fixture");
    let command = test_command_script("verification plan recovery");

    let verification_plan = |request: &str, call_id: &str, sandbox_mode: &str| {
        let mut response = ModelTurnResponse::completed(request, call_id, "");
        response.tool_calls.push(tool_call(
            call_id,
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "verify the changed fixture", "status": "completed"}],
                "verification": [{
                    "risk": "general_mutation",
                    "evidence": "the fixture changed",
                    "affected_path": "README.md",
                    "affected_symbol": "README.md::value",
                    "current_gap": "the changed revision is not verified",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": sandbox_mode,
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        response
    };
    let mut mutation = ModelTurnResponse::completed(
        "model_request_turn_verification_recovery_0",
        "response_mutation",
        "",
    );
    mutation.tool_calls.push(tool_call(
        "edit_verification_recovery",
        "edit",
        serde_json::json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut command_before_plan = ModelTurnResponse::completed(
        "model_request_turn_verification_recovery_2",
        "response_command_before_plan",
        "",
    );
    command_before_plan.tool_calls.push(tool_call(
        "command_before_plan",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut command_after_plan = ModelTurnResponse::completed(
        "model_request_turn_verification_recovery_5",
        "response_command_after_plan",
        "",
    );
    command_after_plan.tool_calls.push(tool_call(
        "command_after_plan",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                mutation,
                verification_plan(
                    "model_request_turn_verification_recovery_1",
                    "wrong_plan_1",
                    "read_only",
                ),
                command_before_plan,
                verification_plan(
                    "model_request_turn_verification_recovery_3",
                    "wrong_plan_2",
                    "read_only",
                ),
                verification_plan(
                    "model_request_turn_verification_recovery_4",
                    "valid_plan",
                    "workspace_write",
                ),
                command_after_plan,
                ModelTurnResponse::completed(
                    "model_request_turn_verification_recovery_6",
                    "response_final",
                    "done",
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind verification recovery workspace")
            .with_sandbox_backend(ExecutionCountingBackend {
                calls: Arc::clone(&backend_calls),
            }),
    );
    let input = AgentLoopInput::new(
        "thread_verification_recovery",
        "turn_verification_recovery",
        "change and verify the fixture",
    )
    .with_max_turns(7);
    let mut checkpoints = Vec::new();
    let result =
        agent_loop.run_with_events_and_checkpoints(&input, &mut |_event| Ok(()), &mut |event| {
            checkpoints.push(event);
            Ok(())
        });

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
    assert!(result.tool_results.iter().any(|result| {
        result.tool_name == "command" && result.error_code.as_deref() == Some("tool_not_visible")
    }));
    assert_eq!(
        result
            .tool_results
            .iter()
            .filter(|result| result.error_code.as_deref() == Some("invalid_tool_arguments"))
            .count(),
        2
    );
    let valid_plan_checkpoint = checkpoints
        .iter()
        .find(|event| {
            matches!(
                &event.phase,
                TurnCheckpointPhase::ToolResultsCommitted { tool_call_ids }
                    if tool_call_ids == &["valid_plan".to_string()]
            )
        })
        .expect("valid verification plan checkpoint");
    assert!(valid_plan_checkpoint.checkpoint.encode().is_ok());
    assert_eq!(
        valid_plan_checkpoint
            .checkpoint
            .encode()
            .expect("encode valid plan checkpoint")["repair_attempts"],
        0
    );
    let requests = seen_requests
        .lock()
        .expect("verification recovery requests");
    let planning_feedback = requests
        .iter()
        .find_map(|request| {
            request.messages.iter().find(|message| {
                message.role == ModelRole::Developer
                    && message.content.contains("current_session_profile=")
            })
        })
        .expect("current session profile guidance");
    assert!(
        planning_feedback
            .content
            .contains("current_session_profile=")
    );
    assert!(
        planning_feedback
            .content
            .contains("\"sandbox_mode\":\"workspace_write\"")
    );
    assert!(
        planning_feedback
            .content
            .contains("\"network_access\":\"denied\"")
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&input);

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_turns, 2);
    assert_eq!(result.tool_results.len(), 1);
    assert!(result.tool_results[0].ok);
    assert_eq!(result.final_answer.as_deref(), Some("done"));
}

#[test]
fn duplicate_tool_call_ids_keep_distinct_occurrence_ordinals_without_execution() {
    let mut response = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    response.tool_calls = vec![
        tool_call(
            "duplicate",
            "read",
            serde_json::json!({"path": "README.md"}),
        ),
        tool_call(
            "duplicate",
            "read",
            serde_json::json!({"path": "README.md"}),
        ),
    ];
    let mut events = Vec::new();

    let result = agent_loop_with_response(response, allow_read_policy()).run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "read").with_max_turns(1),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(result.tool_results.is_empty());
    let finished = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
                if matches!(value.lifecycle, OccurrenceLifecycle::Finished { .. }) =>
            {
                Some(value)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    assert_eq!(
        finished[0].tool_call_id_digest,
        finished[1].tool_call_id_digest
    );
    assert_ne!(
        finished[0].identity.occurrence_id,
        finished[1].identity.occurrence_id
    );
    assert_eq!(finished[0].tool_call_ordinal, 0);
    assert_eq!(finished[1].tool_call_ordinal, 1);
}

#[test]
fn event_aware_command_run_links_tool_policy_sandbox_verification_and_final_review() {
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = "token=runtime-observation-secret";
    let command = format!("test-program {secret}");
    let input = AgentLoopInput::new("thread_1", "turn_1", "run command")
        .with_max_turns(2)
        .with_verification_commands([verification_command(command.clone(), 1)]);
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({"command": command, "timeout_seconds": 5}),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_command",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );
    let mut events = Vec::new();

    let result = agent_loop_with_responses_and_requests(
        vec![
            command_response,
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
        ],
        policy,
        Arc::new(Mutex::new(Vec::new())),
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_events(&input, &mut |event| {
        events.push(event);
        Ok(())
    });

    assert_eq!(result.status, AgentStatus::Completed);
    let tool = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::ToolCall(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool.len(), 2);
    assert_eq!(
        tool[0].identity.occurrence_id,
        tool[1].identity.occurrence_id
    );
    assert!(matches!(
        tool[0].lifecycle,
        OccurrenceLifecycle::Started { .. }
    ));
    assert!(matches!(
        tool[1].lifecycle,
        OccurrenceLifecycle::Finished {
            status: ToolCallStatus::Succeeded,
            ..
        }
    ));
    let prompt_parent = events
        .iter()
        .find_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::PromptAssembly(value))
                if value.model_turn_ordinal == 0
                    && matches!(value.lifecycle, OccurrenceLifecycle::Finished { .. }) =>
            {
                Some(value.identity.occurrence_id.clone())
            }
            _ => None,
        })
        .expect("tool request prompt parent");
    assert_eq!(tool[0].identity.parent_occurrence_id, Some(prompt_parent));

    let policy = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::PolicyDecision(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(policy.len(), 2);
    assert_eq!(policy[1].cause, Some(PolicyDecisionCause::Rule));
    assert!(matches!(
        policy[1].lifecycle,
        OccurrenceLifecycle::Finished {
            status: PolicyDecisionStatus::Allow,
            ..
        }
    ));

    let sandbox = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::SandboxExecution(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(sandbox.len(), 2);
    assert_eq!(
        sandbox[0].identity.parent_occurrence_id,
        Some(tool[0].identity.occurrence_id.clone())
    );
    assert_eq!(sandbox[0].command_id, sandbox[1].command_id);
    assert_eq!(sandbox[1].command_id_binding_valid, Some(true));
    assert!(matches!(
        sandbox[1].lifecycle,
        OccurrenceLifecycle::Finished {
            status: SandboxExecutionStatus::Ok,
            ..
        }
    ));

    let verification = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::Verification(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(verification.iter().any(|value| matches!(
        value.lifecycle,
        OccurrenceLifecycle::Finished {
            status: VerificationStatus::CommandPassed,
            ..
        }
    )));
    assert!(verification.iter().any(|value| matches!(
        value.lifecycle,
        OccurrenceLifecycle::Finished {
            status: VerificationStatus::GatePassed,
            ..
        }
    )));

    let final_review = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::FinalReview(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(final_review.len(), 2);
    assert!(matches!(
        final_review[1].lifecycle,
        OccurrenceLifecycle::Finished {
            status: FinalReviewStatus::Succeeded,
            ..
        }
    ));
    assert!(
        !serde_json::to_string(&events)
            .expect("serialize runtime observations")
            .contains(secret)
    );
}

#[test]
fn sandbox_start_sink_failure_stops_before_backend_execution() {
    let dir = tempfile::tempdir().expect("temp dir");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "call_1",
        "command",
        serde_json::json!({"command": "test-program safe", "timeout_seconds": 5}),
    ));
    let policy = PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_command",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Execute),
    );
    let mut events = Vec::new();

    let result = agent_loop_with_response(command_response, policy)
        .with_workspace_tools(
            WorkspaceTools::new(dir.path())
                .expect("bind workspace tools")
                .with_sandbox_backend(ExecutionCountingBackend {
                    calls: Arc::clone(&calls),
                }),
        )
        .run_with_events(
            &AgentLoopInput::new("thread_1", "turn_1", "run command"),
            &mut |event| {
                let reject = matches!(
                    event,
                    AgentLoopEvent::Observation(AgentObservation::SandboxExecution(ref value))
                        if matches!(value.lifecycle, OccurrenceLifecycle::Started { .. })
                );
                events.push(event);
                if reject {
                    Err(AgentLoopEventSinkError)
                } else {
                    Ok(())
                }
            },
        );

    assert_eq!(result.status, AgentStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("agent event sink failed"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AgentLoopEvent::Observation(AgentObservation::SandboxExecution(_))
            ))
            .count(),
        1
    );
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentFailThenSucceedBackend {
                calls: AtomicUsize::new(0),
            }),
    )
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
    let tool_message = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool result message");
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentExecutableUnavailableBackend {
                calls: AtomicUsize::new(0),
            }),
    )
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
    let tool_message = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool result message");
    assert_eq!(tool_message.tool_call_id.as_deref(), Some("call_1"));
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
    let workspace = WorkspaceTools::new(dir.path())
        .expect("bind workspace tools")
        .with_sandbox_backend(BlockingCommandBackend {
            started: Mutex::new(Some(started_tx)),
        });
    let events = Arc::new(Mutex::new(Vec::new()));
    let worker_events = Arc::clone(&events);
    let worker = thread::spawn(move || {
        agent_loop_with_response(command_response, policy)
            .with_workspace_tools(workspace)
            .with_cancellation_token(worker_cancellation)
            .run_with_events(
                &AgentLoopInput::new("thread_1", "turn_1", "run command"),
                &mut |event| {
                    worker_events.lock().expect("event lock").push(event);
                    Ok(())
                },
            )
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
    let events = events.lock().expect("event lock");
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::SandboxExecution(value))
            if matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                status: SandboxExecutionStatus::Cancelled,
                ..
            })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::ToolCall(value))
            if matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                status: ToolCallStatus::Cancelled,
                ..
            })
    )));
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
            WorkspaceTools::new(dir.path())
                .expect("bind workspace tools")
                .with_sandbox_backend(AgentStrictBackend),
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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );

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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );

    let result = agent_loop.run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.pending_approvals[0].request().resources,
        vec![command_resource]
    );
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    let pending = pending_approval(&result);
    let pending_arguments: serde_json::Value =
        serde_json::from_str(&pending.pending_tool_call().raw_arguments)
            .expect("pending arguments");
    assert_eq!(pending_arguments["cwd"], ".");
    assert_eq!(pending_arguments["timeout_seconds"], 5);
    assert_eq!(pending_arguments["command"], command_script);

    let mut tampered_arguments = pending_arguments;
    tampered_arguments["command"] = serde_json::json!("different command");
    let mut checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    checkpoint["raw_arguments"] = serde_json::json!(tampered_arguments.to_string());
    let tampered =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
            .expect("tampered typed occurrence");
    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let resumed = agent_loop.resume_pending_approval(&resumed_input, &tampered);

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
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
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

#[test]
fn workspace_write_command_mutation_invalidates_stale_verification_evidence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let file_path = dir.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let mut edit_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    edit_response.tool_calls.push(tool_call(
        "edit_call",
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
        "verification_call",
        "command",
        serde_json::json!({"command": "test-program verify", "timeout_seconds": 5}),
    ));
    let premature_final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    let mut post_mutation_verification_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "");
    post_mutation_verification_response
        .tool_calls
        .push(tool_call(
            "post_mutation_verification_call",
            "command",
            serde_json::json!({"command": "test-program verify", "timeout_seconds": 5}),
        ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_4", "response_5", "done");
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let mut events = Vec::new();
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit_response,
                verification_response,
                premature_final_response,
                post_mutation_verification_response,
                final_response,
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(dir.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(CommandMutatingBackend {
                workspace: dir.path().to_path_buf(),
                calls: AtomicUsize::new(0),
                include_summary: true,
            }),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "edit and verify")
            .with_max_turns(5)
            .with_verification_commands([verification_command("test-program verify", 1)]),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(
        result.status,
        AgentStatus::Completed,
        "error={:?} tool_results={:?}",
        result.error,
        result.tool_results
    );
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.tool_results.len(), 3);
    assert_eq!(result.verification.required_command_count, 1);
    assert_eq!(result.verification.satisfied_command_count, 1);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
    let verification_events = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::Verification(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(verification_events.len() >= 4);
    for pair in verification_events.chunks_exact(2) {
        assert_eq!(pair[0].identity, pair[1].identity);
        assert_eq!(
            pair[0].required_command_count,
            pair[1].required_command_count
        );
        assert_eq!(pair[0].occurrence_count, pair[1].occurrence_count);
    }
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read file"),
        "command mutation"
    );
}

#[test]
fn verification_plan_occurrences_keep_distinct_identity_after_failed_initial_check() {
    let workspace = tempfile::tempdir().expect("workspace");
    let command = test_command_script("verification_identity");
    let verification_command = verification_command(command.clone(), 1);
    let mut failed_command = ModelTurnResponse::completed(
        "model_request_turn_verification_identity_0",
        "response_identity",
        "",
    );
    failed_command.tool_calls.push(tool_call(
        "failed_verification_identity",
        "command",
        serde_json::json!({
            "command": command,
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut events = Vec::new();
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![failed_command],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentAlwaysFailBackend),
    )
    .run_with_events(
        &AgentLoopInput::new(
            "thread_verification_identity",
            "turn_verification_identity",
            "run the verification",
        )
        .with_max_turns(1)
        .with_verification_commands([verification_command]),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    let plan_events = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::VerificationPlan(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(plan_events.len(), 4, "result={result:?}");
    assert!(matches!(
        plan_events[0].lifecycle,
        OccurrenceLifecycle::Started { .. }
    ));
    assert!(matches!(
        plan_events[1].lifecycle,
        OccurrenceLifecycle::Finished {
            status: VerificationPlanStatus::Rejected,
            ..
        }
    ));
    assert!(matches!(
        plan_events[2].lifecycle,
        OccurrenceLifecycle::Started { .. }
    ));
    assert!(matches!(
        plan_events[3].lifecycle,
        OccurrenceLifecycle::Finished {
            status: VerificationPlanStatus::Planned,
            ..
        }
    ));
    assert_eq!(
        plan_events[0].identity.occurrence_id,
        plan_events[1].identity.occurrence_id
    );
    assert_eq!(
        plan_events[2].identity.occurrence_id,
        plan_events[3].identity.occurrence_id
    );
    assert_ne!(
        plan_events[0].identity.occurrence_id,
        plan_events[2].identity.occurrence_id
    );
}

#[test]
fn command_mutation_keeps_verification_span_identity_after_plan_invalidation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let fixture_name = "README.md";
    std::fs::write(workspace.path().join(fixture_name), "before").expect("write fixture");
    let command = test_command_script("mutating_verification");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_verification_span_0",
        "response_span_edit",
        "",
    );
    edit.tool_calls.push(tool_call(
        "span_edit",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_verification_span_1",
        "response_span_plan",
        "",
    );
    plan.tool_calls.push(tool_call(
        "span_plan",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the mutation", "status": "in_progress"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "the fixture changed",
                "affected_path": fixture_name,
                "affected_symbol": "README::content",
                "current_gap": "the changed revision is not verified",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut verification = ModelTurnResponse::completed(
        "model_request_turn_verification_span_2",
        "response_span_command",
        "",
    );
    verification.tool_calls.push(tool_call(
        "span_command",
        "command",
        serde_json::json!({
            "command": command,
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let mut events = Vec::new();

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![edit, plan, verification],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(CommandMutatingBackend {
                workspace: workspace.path().to_path_buf(),
                calls: AtomicUsize::new(0),
                include_summary: true,
            }),
    )
    .run_with_events(
        &AgentLoopInput::new(
            "thread_verification_span",
            "turn_verification_span",
            "change and verify the fixture",
        )
        .with_max_turns(3),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Failed, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(fixture_name)).expect("read fixture"),
        "command mutation"
    );
    let verification_events = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::Verification(value)) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(verification_events.len(), 2, "{result:?}");
    assert_eq!(
        verification_events[0].identity,
        verification_events[1].identity
    );
    assert_eq!(
        verification_events[0].required_command_count,
        verification_events[1].required_command_count
    );
    assert_eq!(
        verification_events[0].occurrence_count,
        verification_events[1].occurrence_count
    );
}

#[test]
fn dynamic_verification_fails_closed_when_command_omits_trusted_change_summary() {
    let workspace = tempfile::tempdir().expect("workspace");
    let file_path = workspace.path().join("README.md");
    std::fs::write(&file_path, "before").expect("write file");
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(tool_call(
        "mutating_command",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![command_response],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(CommandMutatingBackend {
                workspace: workspace.path().to_path_buf(),
                calls: AtomicUsize::new(0),
                include_summary: false,
            }),
    )
    .run(&AgentLoopInput::new(
        "thread_1",
        "turn_1",
        "run the mutating command",
    ));

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    assert!(result.error.as_deref().is_some_and(|error| error.contains(
        "workspace mutation did not provide a trusted changed-files and diff digest summary"
    )));
    assert_eq!(
        std::fs::read_to_string(file_path).expect("read mutated file"),
        "command mutation"
    );
}

struct AgentStrictBackend;

struct ExecutionCountingBackend {
    calls: Arc<AtomicUsize>,
}

impl SandboxBackend for ExecutionCountingBackend {
    fn name(&self) -> &'static str {
        "execution_counting_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, _request: &CommandRequest) -> CommandResult {
        panic!("direct argv command backend must not execute")
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        CommandResult::completed(&request.command_id, "command ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

struct CommandMutatingBackend {
    workspace: PathBuf,
    calls: AtomicUsize,
    include_summary: bool,
}

impl SandboxBackend for CommandMutatingBackend {
    fn name(&self) -> &'static str {
        "command_mutating_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "direct command")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        let changed = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let mut summary = None;
        if changed {
            let path = self.workspace.join("README.md");
            let before = std::fs::read(&path).expect("read before command mutation");
            let after = b"command mutation";
            std::fs::write(&path, after).expect("command mutation");
            let mut hasher = Sha256::new();
            hasher.update(b"README.md");
            hasher.update(&before);
            hasher.update(after);
            if self.include_summary {
                summary = Some(WorkspaceChangeSummary::new(
                    vec!["README.md".to_string()],
                    format!("sha256:{:x}", hasher.finalize()),
                ));
            }
        }
        let result = CommandResult::completed(&request.command_id, "command ok")
            .with_workspace_mutation(if changed {
                WorkspaceMutation::Changed
            } else {
                WorkspaceMutation::Unchanged
            })
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            );
        match summary {
            Some(summary) => result.with_workspace_change_summary(summary),
            None => result,
        }
    }
}

impl SandboxBackend for AgentStrictBackend {
    fn name(&self) -> &'static str {
        "agent_strict_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "agent command ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "agent command ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
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
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "large-safe-output\n".repeat(2_000))
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "large-safe-output\n".repeat(2_000))
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

#[derive(Default)]
struct SequencedOutputBackend {
    calls: AtomicUsize,
}

impl SequencedOutputBackend {
    fn output(&self) -> String {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            "small-safe-output".to_string()
        } else {
            "large-safe-output\n".repeat(2_000)
        }
    }
}

impl SandboxBackend for SequencedOutputBackend {
    fn name(&self) -> &'static str {
        "sequenced_output_strict_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, self.output())
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, self.output())
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
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
        SandboxCapabilities::strict().with_change_detection()
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
        CommandResult::cancelled(&request.command_id, 1)
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
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
        CommandResult::cancelled(&request.command_id, 1)
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

struct AgentFailThenSucceedBackend {
    calls: AtomicUsize,
}

struct AgentAlwaysFailBackend;

impl SandboxBackend for AgentAlwaysFailBackend {
    fn name(&self) -> &'static str {
        "agent_always_fail_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

impl SandboxBackend for AgentFailThenSucceedBackend {
    fn name(&self) -> &'static str {
        "agent_fail_then_succeed_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        } else {
            CommandResult::completed(&request.command_id, "repaired")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(
                    self.name(),
                    singularity_tools::SandboxBackendEnforcement::Strict,
                )
        }
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        let result = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executed(&request.command_id, 1, 1, "", "failed", false)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
        } else {
            CommandResult::completed(&request.command_id, "repaired")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
        };
        result.with_sandbox_execution(
            self.name(),
            singularity_tools::SandboxBackendEnforcement::Strict,
        )
    }
}

struct AgentSemanticFixtureBackend {
    file_path: PathBuf,
}

impl AgentSemanticFixtureBackend {
    fn result(&self, command_id: &str) -> CommandResult {
        let output_dir = tempfile::tempdir().expect("semantic fixture output");
        let executable = output_dir
            .path()
            .join(format!("semantic_fixture{}", std::env::consts::EXE_SUFFIX));
        let compile = std::process::Command::new("rustc")
            .arg("--edition=2021")
            .args(["--crate-name", "semantic_fixture"])
            .arg(&self.file_path)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("execute rustc for semantic fixture");
        let result = if compile.status.success() {
            let executed = std::process::Command::new(&executable)
                .output()
                .expect("execute semantic fixture");
            CommandResult::executed(
                command_id,
                executed.status.code().unwrap_or(1),
                1,
                String::from_utf8_lossy(&executed.stdout),
                String::from_utf8_lossy(&executed.stderr),
                false,
            )
        } else {
            CommandResult::executed(
                command_id,
                compile.status.code().unwrap_or(1),
                1,
                String::from_utf8_lossy(&compile.stdout),
                String::from_utf8_lossy(&compile.stderr),
                false,
            )
        };
        result
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

impl SandboxBackend for AgentSemanticFixtureBackend {
    fn name(&self) -> &'static str {
        "agent_semantic_fixture_test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        self.result(&request.command_id)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        self.result(&request.command_id)
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
        SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        let result = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            CommandResult::executable_unavailable(
                &request.command_id,
                "required executable 'missing-host-tool' was not found on host PATH",
            )
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
        } else {
            CommandResult::completed(&request.command_id, "repaired")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
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
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
        } else {
            CommandResult::completed(&request.command_id, "repaired")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
    .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(result.approval_count, 1);
    assert_eq!(
        result.pending_approvals[0].request().request_id,
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
        .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
        .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
        .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
        .run(&input);

    assert_eq!(result.status, AgentStatus::Blocked);
    assert_eq!(
        result.tool_results[0].error_code.as_deref(),
        Some("approval_required")
    );
    assert_eq!(
        result.pending_approvals[0].request().request_id,
        "approval_turn_1_call_1"
    );
    assert_eq!(result.pending_approvals[0].request().thread_id, "thread_1");
    assert_eq!(result.pending_approvals[0].request().turn_id, "turn_1");
    assert_eq!(
        result.pending_approvals[0]
            .request()
            .tool_call_id
            .as_deref(),
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
    .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
        .with_workspace_tools(WorkspaceTools::new(dir.path()).expect("bind workspace tools"))
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
fn plan_shape_failure_explains_json_structure_without_echoing_input() {
    let invalid_result = |arguments: serde_json::Value| {
        let mut response = ModelTurnResponse::completed(
            "model_request_turn_plan_shape_0",
            "response_plan_shape_0",
            "",
        );
        response
            .tool_calls
            .push(tool_call("invalid_plan_shape", "update_plan", arguments));
        agent_loop_with_capabilities_and_plan(
            vec![response],
            allow_read_policy(),
            Arc::new(Mutex::new(Vec::new())),
            ProviderProtocolContract::default(),
            true,
        )
        .run(
            &AgentLoopInput::new("thread_plan_shape", "turn_plan_shape", "update the plan")
                .with_max_turns(1),
        )
        .tool_results[0]
            .to_message_payload()
    };

    let shape_payload = invalid_result(serde_json::json!({"steps": "SENSITIVE_PLAN_PAYLOAD"}));
    let shape_summary = shape_payload["content"]["summary"]
        .as_str()
        .expect("plan shape summary");
    assert!(shape_summary.contains("steps field is an array"));
    assert!(shape_summary.contains("Do not encode"));
    assert!(!shape_summary.contains("SENSITIVE_PLAN_PAYLOAD"));

    let path_payload = invalid_result(serde_json::json!({
        "steps": [{"step": "verify", "status": "in_progress"}],
        "verification": [{
            "risk": "general_mutation",
            "evidence": "verify the mutation",
            "affected_path": "calculator.py",
            "affected_symbol": "multiline_total",
            "current_gap": "tests have not run",
            "action": {
                "command": "python -m unittest",
                "cwd": "/workspace",
                "timeout_seconds": 30,
                "sandbox_mode": "workspace_write",
                "network_access": "denied"
            },
        }]
    }));
    let path_summary = path_payload["content"]["summary"]
        .as_str()
        .expect("plan path summary");
    assert!(path_summary.contains("workspace-relative"));
    assert!(path_summary.contains("cwd \".\""));
    assert!(!path_summary.contains("/workspace"));
}

#[test]
fn agent_binds_provider_runtime_observations_to_each_prompt_assembly() {
    let mut first = ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    first.tool_calls.push(plan_tool_call(
        "plan_call_1",
        serde_json::json!([{"step": "inspect", "status": "in_progress"}]),
    ));
    first.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 2,
        retry_count: 1,
        occurrences: vec![
            provider_attempt_occurrence(91, "provider-first", ProviderAttemptStatus::Error),
            provider_attempt_occurrence(92, "provider-first-retry", ProviderAttemptStatus::Ok),
        ],
        ..Default::default()
    });
    first.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Miss,
            observed_at_unix_ms: 6,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });

    let mut complete_plan_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    complete_plan_response.tool_calls.push(plan_tool_call(
        "plan_call_2",
        serde_json::json!([{"step": "inspect", "status": "completed"}]),
    ));
    complete_plan_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        occurrences: vec![provider_attempt_occurrence(
            93,
            "provider-plan-complete",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    complete_plan_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Hit,
            observed_at_unix_ms: 7,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });

    let mut final_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "done");
    final_response.provider_attempt_metadata = Some(ProviderAttemptMetadata {
        attempt_count: 1,
        occurrences: vec![provider_attempt_occurrence(
            94,
            "provider-final",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    final_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Hit,
            observed_at_unix_ms: 8,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });

    let mut events = Vec::new();
    let agent = agent_loop_with_plan_capabilities(
        vec![first, complete_plan_response, final_response],
        allow_read_policy(),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract::default(),
    );
    let result = agent.run_with_events(
        &AgentLoopInput::new("thread_runtime", "turn_runtime", "inspect"),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    let prompt_ids = events.iter().fold(Vec::new(), |mut prompt_ids, event| {
        if let AgentLoopEvent::Observation(AgentObservation::PromptAssembly(observation)) = event
            && !prompt_ids.contains(&observation.identity.occurrence_id)
        {
            prompt_ids.push(observation.identity.occurrence_id.clone());
        }
        prompt_ids
    });
    assert_eq!(prompt_ids.len(), 1);
    assert_eq!(result.model_turns, 1);
    assert_eq!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| {
                (
                    occurrence.model_turn_ordinal,
                    occurrence.parent_occurrence_id.clone(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (Some(0), Some(prompt_ids[0].clone())),
            (Some(0), Some(prompt_ids[0].clone())),
        ]
    );
    let capability = result
        .provider_capability_metadata
        .as_ref()
        .expect("aggregated capability metadata");
    assert_eq!(
        capability
            .cache_observations
            .iter()
            .map(|observation| (
                observation.outcome,
                observation.model_turn_ordinal,
                observation.parent_occurrence_id.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![(
            ProviderCapabilityCacheLookupResult::Miss,
            Some(0),
            Some(prompt_ids[0].clone()),
        ),]
    );
    let public = serde_json::to_string(&result).expect("serialize result");
    assert!(!public.contains("cache_observations"));
    assert!(!public.contains("provider_capability_metadata"));
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
        occurrences: vec![
            provider_attempt_occurrence(99, "provider-plan-first", ProviderAttemptStatus::Ok),
            provider_attempt_occurrence(99, "provider-plan-retry", ProviderAttemptStatus::Error),
        ],
        ..Default::default()
    });
    plan_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Miss,
            observed_at_unix_ms: 9,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
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
        occurrences: vec![provider_attempt_occurrence(
            77,
            "provider-final",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    final_response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: vec![ProviderCapabilityCacheObservation {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            outcome: ProviderCapabilityCacheLookupResult::Hit,
            observed_at_unix_ms: 10,
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        }],
        ..negotiated_capability_metadata()
    });

    let mut events = Vec::new();
    let result = agent_loop_with_plan_capabilities(
        vec![plan_response, final_response],
        allow_read_policy(),
        Arc::new(Mutex::new(Vec::new())),
        ProviderProtocolContract::default(),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_1", "turn_1", "inspect"),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.model_usage.input_tokens, 240);
    assert_eq!(result.model_usage.output_tokens, 30);
    assert_eq!(result.model_usage.total_tokens, 270);
    assert_eq!(result.model_usage.cached_input_tokens, 70);
    assert_eq!(result.model_usage.reasoning_tokens, 7);
    assert_eq!(result.provider_attempts.attempt_count, 3);
    assert_eq!(result.provider_attempts.retry_count, 1);
    assert_eq!(result.provider_attempts.latency_ms, 105);
    assert_eq!(result.model_turns, 2);
    assert_eq!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| occurrence.attempt_index)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| occurrence.provider_name.as_str())
            .collect::<Vec<_>>(),
        [
            "provider-plan-first",
            "provider-plan-retry",
            "provider-final"
        ]
    );
    let prompt_ids = events.iter().fold(Vec::new(), |mut prompt_ids, event| {
        if let AgentLoopEvent::Observation(AgentObservation::PromptAssembly(observation)) = event
            && !prompt_ids.contains(&observation.identity.occurrence_id)
        {
            prompt_ids.push(observation.identity.occurrence_id.clone());
        }
        prompt_ids
    });
    assert_eq!(prompt_ids.len(), 2);
    assert_eq!(
        result
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.model_turn_ordinal,
                occurrence.parent_occurrence_id.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(0), Some(prompt_ids[0].clone())),
            (Some(0), Some(prompt_ids[0].clone())),
            (Some(1), Some(prompt_ids[1].clone())),
        ]
    );
    let capability = result
        .provider_capability_metadata
        .as_ref()
        .expect("capability observation aggregate");
    assert_eq!(
        capability
            .cache_observations
            .iter()
            .map(|observation| (
                observation.outcome,
                observation.model_turn_ordinal,
                observation.parent_occurrence_id.clone(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                ProviderCapabilityCacheLookupResult::Miss,
                Some(0),
                Some(prompt_ids[0].clone()),
            ),
            (
                ProviderCapabilityCacheLookupResult::Hit,
                Some(1),
                Some(prompt_ids[1].clone()),
            ),
        ]
    );
    let status = result.to_run_status();
    assert_eq!(status.model_usage, result.model_usage);
    assert_eq!(status.provider_attempts, result.provider_attempts);
    let public_result = serde_json::to_string(&result).expect("serialize public result");
    assert!(!public_result.contains("occurrences"));
    let public_status = serde_json::to_string(&status).expect("serialize public status");
    assert!(!public_status.contains("occurrences"));
    let result_schema = serde_json::to_value(schemars::schema_for!(AgentLoopResult))
        .expect("serialize result schema");
    assert!(
        result_schema["definitions"]["ProviderAttemptMetadata"]["properties"]
            .get("occurrences")
            .is_none()
    );
    let status_schema = serde_json::to_value(schemars::schema_for!(AgentRunStatus))
        .expect("serialize status schema");
    assert!(
        status_schema["definitions"]["ProviderAttemptMetadata"]["properties"]
            .get("occurrences")
            .is_none()
    );
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
    let verification_entry = &spec.input_schema["properties"]["verification"]["items"];
    assert!(
        verification_entry["properties"].get("required").is_none(),
        "model verification entries must not control repeat counts"
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

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
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
    let verification_argv = test_command("verify");
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
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new("thread_1", "turn_1", "finish and verify")
            .with_max_turns(3)
            .with_verification_commands([verification_command(verification_argv.join(" "), 1)]),
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
fn verification_plan_repair_review_completion_closes_boundary_fixture_matrix() {
    let fixtures = vec![
        (
            AgentVerificationRisk::EmptyCollection,
            "empty collection.rs",
            "fn total(values: &[i64]) -> i64 { values.iter().sum() }\nfn main() { assert_eq!(total(&[1, 2]), 3); }\n",
            "fn total(values: &[i64]) -> Option<i64> { (!values.is_empty()).then(|| values.iter().sum()) }\nfn main() { let total: i64 = total(&[]); assert_eq!(total, 0); }\n",
            "fn total(values: &[i64]) -> i64 { values.iter().sum() }\nfn main() { let empty_total: i64 = total(&[]); assert_eq!(empty_total, 0); assert_eq!(total(&[1, 2]), 3); }\n",
            "empty collection",
            "empty_collection",
        ),
        (
            AgentVerificationRisk::OptionalNull,
            "optional_null.rs",
            "fn label(value: Option<&str>) -> String { value.unwrap_or_default().trim().to_string() }\nfn main() { assert_eq!(label(Some(\" ready \")), \"ready\"); }\n",
            "fn label(value: Option<&str>) -> String { value.unwrap().trim().to_string() }\nfn main() { assert_eq!(label(None), \"\"); }\n",
            "fn label(value: Option<&str>) -> String { value.unwrap_or_default().trim().to_string() }\nfn main() { assert_eq!(label(None), \"\"); assert_eq!(label(Some(\" ready \")), \"ready\"); }\n",
            "Optional/Null",
            "optional_null",
        ),
        (
            AgentVerificationRisk::DivisionByZero,
            "zero_index_division.rs",
            "fn ratio(total: i64, count: i64) -> i64 { if count == 0 { 0 } else { total / count } }\nfn main() { assert_eq!(ratio(6, 2), 3); }\n",
            "fn ratio(total: i64, count: i64) -> i64 { total / count }\nfn main() { assert_eq!(ratio(1, 0), 0); }\n",
            "fn ratio(total: i64, count: i64) -> i64 { if count == 0 { 0 } else { total / count } }\nfn main() { assert_eq!(ratio(1, 0), 0); assert_eq!(ratio(6, 2), 3); }\n",
            "division by zero",
            "zero_index_division",
        ),
    ];
    #[cfg(unix)]
    let fixtures = {
        let mut fixtures = fixtures;
        fixtures.push((
            AgentVerificationRisk::GeneralMutation,
            " leading\ntrailing.rs ",
            "fn main() { assert_eq!(2 + 2, 4); }\n",
            "fn main() { assert_eq!(2 + 2, 5); }\n",
            "fn main() { assert_eq!(2 + 2, 4); }\n",
            "exact path binding",
            "structured_path",
        ));
        fixtures
    };
    for (
        risk,
        fixture_name,
        fixture_source,
        incomplete_source,
        repaired_source,
        goal,
        command_name,
    ) in fixtures
    {
        let workspace = tempfile::tempdir().expect("fixture workspace");
        let file_path = workspace.path().join(fixture_name);
        std::fs::write(&file_path, fixture_source).expect("fixture source");
        let command = test_command_script(command_name);
        let mut incomplete_patch =
            ModelTurnResponse::completed("model_request_turn_fixture_0", "response_1", "");
        incomplete_patch.tool_calls.push(tool_call(
            "incomplete_patch",
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": "not present",
                "replacement": "after"
            }),
        ));
        let mut repaired_patch =
            ModelTurnResponse::completed("model_request_turn_fixture_1", "response_2", "");
        repaired_patch.tool_calls.push(tool_call(
            "repaired_patch",
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": fixture_source,
                "replacement": incomplete_source
            }),
        ));
        let mut initial_plan =
            ModelTurnResponse::completed("model_request_turn_fixture_2", "response_3", "");
        initial_plan.tool_calls.push(tool_call(
            "initial_verification_plan",
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "repair and verify the changed fixture", "status": "in_progress"}],
                "verification": [{
                    "risk": risk,
                    "evidence": format!("changed {fixture_name}"),
                    "affected_path": fixture_name,
                    "affected_symbol": format!("{fixture_name}::fixture_boundary"),
                    "current_gap": "verification evidence is not yet recorded",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        let mut failed_check =
            ModelTurnResponse::completed("model_request_turn_fixture_3", "response_4", "");
        failed_check.tool_calls.push(tool_call(
            "failed_check",
            "command",
            serde_json::json!({
                "command": command,
                "cwd": ".",
                "timeout_seconds": 5
            }),
        ));
        let mut repaired_patch_again =
            ModelTurnResponse::completed("model_request_turn_fixture_4", "response_5", "");
        repaired_patch_again.tool_calls.push(tool_call(
            "repaired_patch_again",
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": incomplete_source,
                "replacement": repaired_source
            }),
        ));
        // The second mutation must invalidate the first plan and force a fresh, failure-aware
        // entry before the final verification command.
        let mut replanned_patch =
            ModelTurnResponse::completed("model_request_turn_fixture_5", "response_6", "");
        replanned_patch.tool_calls.push(tool_call(
            "replanned_verification",
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "repair and verify the changed fixture", "status": "completed"}],
                "verification": [{
                    "risk": risk,
                    "evidence": format!("changed {fixture_name}"),
                    "affected_path": fixture_name,
                    "affected_symbol": format!("{fixture_name}::fixture_boundary"),
                    "current_gap": "the repaired behavior still lacks passing evidence",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        let mut repaired_check =
            ModelTurnResponse::completed("model_request_turn_fixture_6", "response_7", "");
        repaired_check.tool_calls.push(tool_call(
            "repaired_check",
            "command",
            serde_json::json!({
                "command": command,
                "cwd": ".",
                "timeout_seconds": 5
            }),
        ));
        let final_response =
            ModelTurnResponse::completed("model_request_turn_fixture_7", "response_8", "completed");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let policy = allow_read_execute_policy().with_rule(
            PermissionRule::new(
                "allow_write",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write),
        );
        let mut events = Vec::new();
        let result = AgentLoop::new(
            StaticProvider {
                responses: vec![
                    incomplete_patch,
                    repaired_patch,
                    initial_plan,
                    failed_check,
                    repaired_patch_again,
                    replanned_patch,
                    repaired_check,
                    final_response,
                ],
                seen_requests: Arc::clone(&seen_requests),
                capabilities: ProviderProtocolContract::default(),
            },
            agent_tool_broker_for_test(true),
            policy,
        )
        .with_workspace_tools(
            WorkspaceTools::new(workspace.path())
                .expect("workspace tools")
                .with_sandbox_backend(AgentSemanticFixtureBackend {
                    file_path: file_path.clone(),
                }),
        )
        .run_with_events(
            &AgentLoopInput::new("thread_fixture", "turn_fixture", goal).with_max_turns(8),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        );

        assert_eq!(
            result.status,
            AgentStatus::Completed,
            "error={:?} tools={:?} verification={:?}",
            result.error,
            result
                .tool_results
                .iter()
                .map(|tool| (&tool.tool_name, &tool.error_code, tool.ok))
                .collect::<Vec<_>>(),
            result.verification
        );
        assert_eq!(
            result.verification.final_review_verdict,
            Some(FinalReviewVerdict::Accept)
        );
        assert_eq!(result.final_answer.as_deref(), Some("completed"));
        assert_eq!(result.verification.required_command_count, 1);
        assert_eq!(
            std::fs::read_to_string(file_path).expect("fixture result"),
            repaired_source
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::VerificationPlan(value))
                if matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: VerificationPlanStatus::Planned,
                    ..
                }) && value.risk_count == 1
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::VerificationPlan(value))
                if matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                    status: VerificationPlanStatus::Rejected,
                    ..
                }) && value.risk_count == 1
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::RepairPlanning(value))
                if value.reason == AgentRepairReason::VerificationFailed
                    && matches!(value.lifecycle, OccurrenceLifecycle::Finished {
                        status: RepairPlanningStatus::Planned,
                        ..
                    })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::FinalReview(value))
                if value.verdict == Some(FinalReviewVerdict::Accept)
        )));
        assert!(result.tool_results.iter().any(
            |tool_result| tool_result.error_code.as_deref() == Some("expected_content_missing")
        ));
        assert!(
            result.tool_results.iter().any(
                |tool_result| tool_result.error_code.as_deref() == Some("command_exit_nonzero")
            )
        );
        let requests = seen_requests.lock().expect("requests");
        assert_eq!(requests.len(), 8);
        assert!(requests[7].tools.is_empty());
        if fixture_name.contains('\n') {
            let planning_feedback = requests[2]
                .messages
                .iter()
                .find(|message| {
                    message.role == ModelRole::Developer
                        && message.content.contains("trusted_change=")
                })
                .expect("structured mutation feedback");
            assert!(!planning_feedback.content.contains(fixture_name));
            assert!(planning_feedback.content.contains("\\n"));
        }
    }
}

#[test]
fn final_review_repair_requires_mutation_replan_and_second_review() {
    let workspace = tempfile::tempdir().expect("review workspace");
    let fixture_name = "review_contract.rs";
    let original = "fn value() -> i32 { 1 }\n";
    let incomplete = "fn value() -> i32 { 2 }\n";
    std::fs::write(workspace.path().join(fixture_name), original).expect("write review fixture");
    let command = test_command_script("success");

    let edit_response = |turn_index: u32, call_id: &str, expected: &str, replacement: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_review_{turn_index}"),
            format!("response_{call_id}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": expected,
                "replacement": replacement
            }),
        ));
        response
    };
    let plan_response = |turn_index: u32, call_id: &str, gap: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_review_{turn_index}"),
            format!("response_{call_id}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "verify the repaired value contract", "status": "completed"}],
                "verification": [{
                    "risk": "zero_value",
                    "evidence": format!("changed {fixture_name}"),
                    "affected_path": fixture_name,
                    "affected_symbol": format!("{fixture_name}::value"),
                    "current_gap": gap,
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        response
    };
    let command_response = |turn_index: u32, call_id: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_review_{turn_index}"),
            format!("response_{call_id}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "command",
            serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
        ));
        response
    };

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let mut events = Vec::new();
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit_response(0, "candidate_edit", original, incomplete),
                plan_response(
                    1,
                    "candidate_plan",
                    "the value contract has not been reviewed",
                ),
                command_response(2, "candidate_check"),
                ModelTurnResponse::completed(
                    "model_request_turn_review_3",
                    "response_review_repair",
                    "__fixture_review_repair__",
                ),
                edit_response(4, "repair_edit", incomplete, original),
                plan_response(
                    5,
                    "repair_plan",
                    "the prior final review rejected the semantic value contract",
                ),
                command_response(6, "repair_check"),
                ModelTurnResponse::completed(
                    "model_request_turn_review_7",
                    "response_review_accept",
                    "completed",
                ),
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind review workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_events(
        &AgentLoopInput::new("thread_review", "turn_review", "restore the value contract")
            .with_max_turns(8),
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.final_answer.as_deref(), Some("completed"));
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
    assert_eq!(
        result.verification.final_review_verdict,
        Some(FinalReviewVerdict::Accept)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(fixture_name)).unwrap(),
        original
    );
    let requests = seen_requests.lock().expect("seen review requests");
    assert_eq!(requests.len(), 8);
    let repair_feedback = requests[4]
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == ModelRole::Developer && message.content.contains("repair_context=")
        })
        .expect("final review repair context");
    assert!(
        repair_feedback
            .content
            .contains("\"repair_strategy_change_required\":true")
    );
    assert!(repair_feedback.content.contains(
        "\"failed_requirement\":\"final review rejected: semantic contract remains incomplete\""
    ));
    assert!(repair_feedback.content.contains(
        "\"previous_result\":\"final review rejected: semantic contract remains incomplete\""
    ));
    assert!(
        repair_feedback
            .content
            .contains("\"affected_path\":\"review_contract.rs\"")
    );
    assert!(
        repair_feedback
            .content
            .contains("\"affected_symbol\":\"review_contract.rs::value\"")
    );
    assert!(
        !repair_feedback
            .content
            .contains("the value contract has not been reviewed")
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::FinalReview(review))
            if review.verdict == Some(FinalReviewVerdict::Repair)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::RepairPlanning(repair))
            if repair.reason == AgentRepairReason::FinalReviewRejected
                && repair.attempt == 1
                && matches!(repair.lifecycle, OccurrenceLifecycle::Finished {
                    status: RepairPlanningStatus::Planned,
                    ..
                })
    )));
}

#[test]
fn missing_verification_repair_constrains_the_next_command_to_the_exact_action() {
    let workspace = tempfile::tempdir().expect("exact repair action workspace");
    let fixture_name = "exact_repair.txt";
    std::fs::write(workspace.path().join(fixture_name), "before")
        .expect("write exact repair fixture");
    let command = test_command_script("success");

    let mut edit =
        ModelTurnResponse::completed("model_request_turn_exact_repair_0", "response_exact_0", "");
    edit.tool_calls.push(tool_call(
        "edit_exact",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan =
        ModelTurnResponse::completed("model_request_turn_exact_repair_1", "response_exact_1", "");
    plan.tool_calls.push(tool_call(
        "plan_exact",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the exact repair fixture", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "changed exact_repair.txt",
                "affected_path": fixture_name,
                "affected_symbol": "exact_repair::value",
                "current_gap": "the exact command has not run",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 10,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut pre_repair_mismatch =
        ModelTurnResponse::completed("model_request_turn_exact_repair_2", "response_exact_2", "");
    pre_repair_mismatch.tool_calls.push(tool_call(
        "command_pre_repair_mismatch",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let premature = ModelTurnResponse::completed(
        "model_request_turn_exact_repair_3",
        "response_exact_3",
        "not verified",
    );
    let mut mismatched =
        ModelTurnResponse::completed("model_request_turn_exact_repair_4", "response_exact_4", "");
    mismatched.tool_calls.push(tool_call(
        "command_mismatch",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut repeated_mismatch =
        ModelTurnResponse::completed("model_request_turn_exact_repair_5", "response_exact_5", "");
    repeated_mismatch.tool_calls.push(tool_call(
        "command_mismatch_again",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut exact =
        ModelTurnResponse::completed("model_request_turn_exact_repair_6", "response_exact_6", "");
    exact.tool_calls.push(tool_call(
        "command_exact",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 10}),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_exact_repair_7",
        "response_exact_7",
        "completed",
    );

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit,
                plan,
                pre_repair_mismatch,
                premature,
                mismatched,
                repeated_mismatch,
                exact,
                final_response,
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind exact repair workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_exact_repair",
            "turn_exact_repair",
            "change and verify the fixture",
        )
        .with_max_turns(8),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert_eq!(result.recovery_metrics.invalid_tool_call_count, 3);
    let run_status = result.to_run_status();
    assert!(run_status.audit_events.iter().any(|event| {
        event["argument_validation_code"] == "repair_action_mismatch"
            && event["executor_started"] == false
    }));
    let requests = seen_requests.lock().expect("exact repair requests");
    for request in [&requests[4], &requests[5], &requests[6]] {
        let command_schema = &request
            .tools
            .iter()
            .find(|tool| tool.name == "command")
            .expect("constrained command tool")
            .parameters_schema;
        assert_eq!(
            command_schema["properties"]["command"]["const"],
            serde_json::json!(command)
        );
        assert_eq!(
            command_schema["properties"]["cwd"]["const"],
            serde_json::json!(".")
        );
        assert_eq!(
            command_schema["properties"]["timeout_seconds"]["const"],
            serde_json::json!(10)
        );
    }
    let conflicting_messages = requests[6]
        .messages
        .iter()
        .filter(|message| {
            message
                .content
                .contains("choose a different next action. Do not repeat the same call")
        })
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert!(
        conflicting_messages.is_empty(),
        "conflicting_messages={conflicting_messages:?}"
    );
    assert!(requests[6].messages.iter().any(|message| {
        message
            .content
            .contains("Do not choose a different action or vary its arguments")
    }));
    assert_eq!(
        requests[6]
            .messages
            .iter()
            .filter(|message| message
                .content
                .starts_with("Follow the bounded repair plan."))
            .count(),
        1
    );
}

#[test]
fn pre_execution_protected_exact_action_releases_pin_for_bounded_replan() {
    let workspace = tempfile::tempdir().expect("pre-execution exact action workspace");
    let fixture_name = "pre_execution_replan.txt";
    std::fs::write(workspace.path().join(fixture_name), "before")
        .expect("write pre-execution fixture");
    let blocked_command = test_command_script("blocked");
    let repaired_command = test_command_script("repaired");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_0",
        "response_pre_execution_replan_0",
        "",
    );
    edit.tool_calls.push(tool_call(
        "edit_pre_execution_replan",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut blocked_plan = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_1",
        "response_pre_execution_replan_1",
        "",
    );
    blocked_plan.tool_calls.push(tool_call(
        "plan_pre_execution_blocked",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the changed fixture", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "changed pre_execution_replan.txt",
                "affected_path": fixture_name,
                "affected_symbol": "pre_execution_replan::value",
                "current_gap": "the verification command has not run",
                "action": {
                    "command": blocked_command,
                    "cwd": ".git",
                    "timeout_seconds": 10,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut blocked = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_2",
        "response_pre_execution_replan_2",
        "",
    );
    blocked.tool_calls.push(tool_call(
        "command_pre_execution_blocked",
        "command",
        serde_json::json!({
            "command": blocked_command,
            "cwd": ".git",
            "timeout_seconds": 10
        }),
    ));
    let mut replan_edit = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_3",
        "response_pre_execution_replan_3",
        "",
    );
    replan_edit.tool_calls.push(tool_call(
        "edit_pre_execution_replan_again",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "after",
            "replacement": "repaired"
        }),
    ));
    let mut repaired_plan = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_4",
        "response_pre_execution_replan_4",
        "",
    );
    repaired_plan.tool_calls.push(tool_call(
        "plan_pre_execution_repaired",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the repaired fixture", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "changed pre_execution_replan.txt",
                "affected_path": fixture_name,
                "affected_symbol": "pre_execution_replan::value",
                "current_gap": "the repaired verification command has not run",
                "action": {
                    "command": repaired_command,
                    "cwd": ".",
                    "timeout_seconds": 10,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut repaired = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_5",
        "response_pre_execution_replan_5",
        "",
    );
    repaired.tool_calls.push(tool_call(
        "command_pre_execution_repaired",
        "command",
        serde_json::json!({
            "command": repaired_command,
            "cwd": ".",
            "timeout_seconds": 10
        }),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_pre_execution_replan_6",
        "response_pre_execution_replan_6",
        "completed",
    );

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit,
                blocked_plan,
                blocked,
                replan_edit,
                repaired_plan,
                repaired,
                final_response,
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind pre-execution workspace")
            .with_sandbox_backend(ExecutionCountingBackend {
                calls: Arc::clone(&backend_calls),
            }),
    )
    .run(
        &AgentLoopInput::new(
            "thread_pre_execution_replan",
            "turn_pre_execution_replan",
            "change and verify the fixture",
        )
        .with_max_turns(7),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
    assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
    assert!(result.tool_results.iter().any(|tool_result| {
        tool_result.error_code.as_deref() == Some("protected_path")
            && tool_result.failure_kind == Some(ToolFailureKind::ProtectedPath)
    }));
    assert!(
        !result
            .tool_results
            .iter()
            .any(|tool_result| tool_result.error_code.as_deref() == Some("repair_action_mismatch"))
    );

    let requests = seen_requests.lock().expect("pre-execution replan requests");
    let blocked_request = &requests[2];
    let blocked_schema = &blocked_request
        .tools
        .iter()
        .find(|tool| tool.name == "command")
        .expect("blocked exact command tool")
        .parameters_schema;
    assert_eq!(
        blocked_schema["properties"]["cwd"]["const"],
        serde_json::json!(".git")
    );

    let replan_request = &requests[3];
    assert!(
        replan_request.tools.iter().any(|tool| tool.name == "edit"),
        "pre-execution boundary must allow a new mutation"
    );
    assert!(
        replan_request
            .tools
            .iter()
            .all(|tool| tool.name != "command" && tool.name != "update_plan"),
        "a strategy-change boundary must not permit repeated verification or replanning: {:?}",
        replan_request.tools
    );
    assert!(
        replan_request.tools.iter().any(|tool| tool.name == "read"),
        "inspection tools must remain available during strategy change"
    );
    let repair_feedback = replan_request
        .messages
        .iter()
        .find(|message| {
            message
                .content
                .starts_with("Follow the bounded repair plan.")
        })
        .expect("bounded replan feedback");
    assert!(
        repair_feedback
            .content
            .contains("\"repair_strategy_change_required\":true"),
        "repair_feedback={}",
        repair_feedback.content
    );
    let repaired_plan_request = &requests[4];
    assert!(
        repaired_plan_request
            .tools
            .iter()
            .all(|tool| tool.name != "command"),
        "a planning prerequisite must hide command until update_plan succeeds"
    );
    assert!(
        repaired_plan_request
            .tools
            .iter()
            .any(|tool| tool.name == "update_plan"),
        "a planning prerequisite must keep update_plan visible"
    );
}

#[test]
fn premature_completion_does_not_orphan_update_plan_input_repair() {
    let workspace = tempfile::tempdir().expect("plan repair workspace");
    let fixture_name = "plan_repair.txt";
    std::fs::write(workspace.path().join(fixture_name), "before").expect("write fixture");
    let command = test_command_script("verified");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_plan_repair_0",
        "response_plan_repair_0",
        "",
    );
    edit.tool_calls.push(tool_call(
        "edit_plan_repair",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let plan_input = |gap: &str, include_verification: bool, status: &str| {
        let mut input = serde_json::json!({
            "steps": [{"step": "repair and verify", "status": status}]
        });
        if include_verification {
            input["verification"] = serde_json::json!([{
                "risk": "general_mutation",
                "evidence": "changed the fixture",
                "affected_path": fixture_name,
                "affected_symbol": "plan_repair::value",
                "current_gap": gap,
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]);
        }
        input
    };
    let plan_response = |turn: u32, call_id: &str, input: serde_json::Value| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_plan_repair_{turn}"),
            format!("response_plan_repair_{turn}"),
            "",
        );
        response
            .tool_calls
            .push(tool_call(call_id, "update_plan", input));
        response
    };
    let mut verification = ModelTurnResponse::completed(
        "model_request_turn_plan_repair_2",
        "response_plan_repair_2",
        "",
    );
    verification.tool_calls.push(tool_call(
        "command_plan_repair",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut unrelated_invalid_command = ModelTurnResponse::completed(
        "model_request_turn_plan_repair_4",
        "response_plan_repair_4",
        "",
    );
    unrelated_invalid_command.tool_calls.push(tool_call(
        "invalid_command_during_plan_repair",
        "command",
        serde_json::json!({"command": 17, "cwd": ".", "timeout_seconds": 5}),
    ));

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit,
                plan_response(
                    1,
                    "install_plan_repair",
                    plan_input("not run", true, "in_progress"),
                ),
                verification,
                plan_response(
                    3,
                    "invalid_plan_repair",
                    plan_input("changed after binding", true, "completed"),
                ),
                unrelated_invalid_command,
                ModelTurnResponse::completed(
                    "model_request_turn_plan_repair_5",
                    "response_plan_repair_5",
                    "done too soon",
                ),
                plan_response(6, "correct_plan_repair", plan_input("", false, "completed")),
                {
                    let mut response = ModelTurnResponse::completed(
                        "model_request_turn_plan_repair_7",
                        "response_plan_repair_7",
                        "",
                    );
                    response.tool_calls.push(tool_call(
                        "command_after_plan_repair",
                        "command",
                        serde_json::json!({
                            "command": command,
                            "cwd": ".",
                            "timeout_seconds": 5
                        }),
                    ));
                    response
                },
                ModelTurnResponse::completed(
                    "model_request_turn_plan_repair_8",
                    "response_plan_repair_8",
                    "__fixture_review_accept__",
                ),
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        allow_read_execute_policy().with_rule(
            PermissionRule::new(
                "allow_write",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Write),
        ),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind plan repair workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_plan_repair",
            "turn_plan_repair",
            "repair the fixture",
        )
        .with_max_turns(9),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert!(result.tool_results.iter().any(|tool_result| {
        tool_result.tool_name == "update_plan"
            && tool_result.error_code.as_deref() == Some("invalid_tool_arguments")
    }));
}

#[test]
fn installed_exact_actions_constrain_each_pre_gate_request_in_order() {
    let workspace = tempfile::tempdir().expect("exact action convergence workspace");
    let fixture_name = "exact_action_convergence.txt";
    std::fs::write(workspace.path().join(fixture_name), "before")
        .expect("write convergence fixture");
    let first_command = test_command_script("first");
    let second_command = test_command_script("second");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_0",
        "response_0",
        "",
    );
    edit.tool_calls.push(tool_call(
        "edit_convergence",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_1",
        "response_1",
        "",
    );
    plan.tool_calls.push(tool_call(
        "plan_convergence",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "run both exact checks", "status": "completed"}],
            "verification": [
                {
                    "risk": "general_mutation",
                    "evidence": "the changed fixture passes its first exact check",
                    "affected_path": fixture_name,
                    "affected_symbol": "exact_action_convergence::first",
                    "current_gap": "the first exact check has not run",
                    "action": {
                        "command": first_command,
                        "cwd": ".",
                        "timeout_seconds": 10,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                },
                {
                    "risk": "optional_null",
                    "evidence": "the changed fixture passes its second exact check",
                    "affected_path": fixture_name,
                    "affected_symbol": "exact_action_convergence::second",
                    "current_gap": "the second exact check has not run",
                    "action": {
                        "command": second_command,
                        "cwd": ".",
                        "timeout_seconds": 10,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }
            ]
        }),
    ));
    let mut first = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_2",
        "response_2",
        "",
    );
    first.tool_calls.push(tool_call(
        "command_convergence_first",
        "command",
        serde_json::json!({
            "command": first_command,
            "cwd": ".",
            "timeout_seconds": 10
        }),
    ));
    let premature = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_3",
        "response_3",
        "completed too early",
    );
    let mut second = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_4",
        "response_4",
        "",
    );
    second.tool_calls.push(tool_call(
        "command_convergence_second",
        "command",
        serde_json::json!({
            "command": second_command,
            "cwd": ".",
            "timeout_seconds": 10
        }),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_exact_action_convergence_5",
        "response_5",
        "completed",
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![edit, plan, first, premature, second, final_response],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind convergence workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new(
            "thread_exact_action_convergence",
            "turn_exact_action_convergence",
            "change and verify the fixture",
        )
        .with_max_turns(5),
    );

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.final_answer.as_deref(), Some("completed"));
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
    let requests = seen_requests.lock().expect("convergence requests");
    let assert_constrained = |request: &ModelTurnRequest, command: &str| {
        assert_eq!(request.tool_choice.mode, ToolChoiceMode::Auto);
        assert_eq!(request.tool_choice.max_tool_calls, 1);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "command");
        let pending_messages = request
            .messages
            .iter()
            .filter(|message| {
                message.role == ModelRole::Developer
                    && message
                        .content
                        .starts_with("Trusted exact verification remains pending.")
            })
            .collect::<Vec<_>>();
        assert_eq!(pending_messages.len(), 1, "request={request:?}");
        assert!(
            pending_messages[0]
                .content
                .contains("submit only the exact command input below as the next action")
        );
        assert!(pending_messages[0].content.contains(command));
        assert_eq!(
            request.tools[0].parameters_schema["properties"]["command"]["const"],
            serde_json::json!(command)
        );
        assert_eq!(
            request.tools[0].parameters_schema["properties"]["cwd"]["const"],
            serde_json::json!(".")
        );
        assert_eq!(
            request.tools[0].parameters_schema["properties"]["timeout_seconds"]["const"],
            serde_json::json!(10)
        );
    };
    assert_constrained(&requests[2], &first_command);
    assert_constrained(&requests[3], &second_command);
    assert_constrained(&requests[4], &second_command);
    assert!(!requests[3].messages.iter().any(|message| {
        message
            .content
            .starts_with("Trusted exact verification remains pending.")
            && message.content.contains(&first_command)
    }));
    assert!(!requests[5].messages.iter().any(|message| {
        message
            .content
            .starts_with("Trusted exact verification remains pending.")
    }));
}

#[test]
fn failed_exact_verification_remains_strategy_change_evidence_after_other_command() {
    let workspace = tempfile::tempdir().expect("failed exact action workspace");
    let fixture_name = "failed_exact_action.txt";
    std::fs::write(workspace.path().join(fixture_name), "before")
        .expect("write failed exact action fixture");
    let exact_command = test_command_script("exact");
    let diagnostic_command = test_command_script("diagnostic");

    let mut edit = ModelTurnResponse::completed(
        "model_request_turn_failed_exact_0",
        "response_failed_exact_0",
        "",
    );
    edit.tool_calls.push(tool_call(
        "edit_failed_exact",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_failed_exact_1",
        "response_failed_exact_1",
        "",
    );
    plan.tool_calls.push(tool_call(
        "plan_failed_exact",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "repair the failed verification", "status": "in_progress"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": "changed failed_exact_action.txt",
                "affected_path": fixture_name,
                "affected_symbol": "failed_exact_action::value",
                "current_gap": "the exact verification still fails",
                "action": {
                    "command": exact_command,
                    "cwd": ".",
                    "timeout_seconds": 10,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut exact = ModelTurnResponse::completed(
        "model_request_turn_failed_exact_2",
        "response_failed_exact_2",
        "",
    );
    exact.tool_calls.push(tool_call(
        "command_failed_exact",
        "command",
        serde_json::json!({
            "command": exact_command,
            "cwd": ".",
            "timeout_seconds": 10
        }),
    ));
    let mut diagnostic = ModelTurnResponse::completed(
        "model_request_turn_failed_exact_3",
        "response_failed_exact_3",
        "",
    );
    diagnostic.tool_calls.push(tool_call(
        "command_diagnostic_after_failure",
        "command",
        serde_json::json!({
            "command": diagnostic_command,
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let stop = ModelTurnResponse::completed(
        "model_request_turn_failed_exact_4",
        "response_failed_exact_4",
        "repair still required",
    );

    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![edit, plan, exact, diagnostic, stop],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind failed exact action workspace")
            .with_sandbox_backend(AgentFailThenSucceedBackend {
                calls: AtomicUsize::new(0),
            }),
    )
    .run(
        &AgentLoopInput::new(
            "thread_failed_exact",
            "turn_failed_exact",
            "repair the failed verification",
        )
        .with_max_turns(5),
    );

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    let requests = seen_requests.lock().expect("failed exact requests");
    for repair_request in [&requests[3], &requests[4]] {
        assert!(!repair_request.messages.iter().any(|message| {
            message
                .content
                .starts_with("Trusted exact verification remains pending.")
        }));
        assert!(
            repair_request
                .tools
                .iter()
                .all(|tool| tool.name != "command" && tool.name != "update_plan"),
            "a later diagnostic must not reopen verification or planning before a changed patch: {:?}",
            repair_request.tools
        );
        let repair_messages = repair_request
            .messages
            .iter()
            .filter(|message| {
                message
                    .content
                    .starts_with("Follow the bounded repair plan.")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            repair_messages.len(),
            1,
            "repair_messages={repair_messages:?}"
        );
        assert!(
            repair_messages[0]
                .content
                .contains("\"required_verification_action\":null"),
            "repair context must permit a materially different strategy after the exact action failed: {}",
            repair_messages[0].content
        );
        assert!(
            repair_messages[0].content.contains("exit_nonzero")
                && repair_messages[0]
                    .content
                    .contains("\"repair_strategy_change_required\":true"),
            "the exact failure must remain the causal evidence: {}",
            repair_messages[0].content
        );
    }
}

#[test]
fn repair_budget_waits_for_new_mutation_and_exposes_bounded_context() {
    let workspace = tempfile::tempdir().expect("repair context workspace");
    let fixture_name = "repair_context.txt";
    std::fs::write(workspace.path().join(fixture_name), "v0").expect("write repair fixture");
    let command = format!("{}{}", test_command_script("failure"), " ".repeat(600));
    let additional_command = test_command_script("additional");
    let expected_scope_digest = command_script_scope_digest_with_policy(
        &command,
        ".",
        10,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let expected_additional_scope_digest = command_script_scope_digest_with_policy(
        &additional_command,
        ".",
        10,
        SandboxFilesystemMode::WorkspaceWrite,
        SandboxNetworkMode::Denied,
    );
    let mut setup = ModelTurnResponse::completed("model_request_turn_context_0", "response_0", "");
    setup.tool_calls.push(tool_call(
        "setup_context",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "v0",
            "replacement": "v1"
        }),
    ));
    let mut plan = ModelTurnResponse::completed("model_request_turn_context_1", "response_1", "");
    plan.tool_calls.push(tool_call(
        "plan_context",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "verify the changed fixture", "status": "completed"}],
            "verification": [
                {
                    "risk": "general_mutation",
                    "evidence": "changed repair_context.txt",
                    "affected_path": fixture_name,
                    "affected_symbol": "repair_context::value",
                    "current_gap": "verification evidence is not yet recorded",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 10,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                },
                {
                    "risk": "optional_null",
                    "evidence": "the changed value also needs an independent boundary check",
                    "affected_path": fixture_name,
                    "affected_symbol": "repair_context::value",
                    "current_gap": "the additional verification evidence is not yet recorded",
                    "action": {
                        "command": additional_command,
                        "cwd": ".",
                        "timeout_seconds": 10,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }
            ]
        }),
    ));
    let failed_command = |turn: u32| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_context_{turn}"),
            format!("response_{turn}"),
            "",
        );
        let call_id = format!("failed_context_{turn}");
        response.tool_calls.push(tool_call(
            &call_id,
            "command",
            serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 15}),
        ));
        response
    };
    let mut unrelated_command =
        ModelTurnResponse::completed("model_request_turn_context_3", "response_3", "");
    unrelated_command.tool_calls.push(tool_call(
        "unrelated_context",
        "command",
        serde_json::json!({
            "command": test_command_script("unrelated"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let mut read = ModelTurnResponse::completed("model_request_turn_context_5", "response_5", "");
    read.tool_calls.push(tool_call(
        "read_context",
        "read",
        serde_json::json!({"path": fixture_name}),
    ));
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy()
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
                "ask_read",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Read)
            .for_resource(workspace_resource(fixture_name)),
        );
    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                setup,
                plan,
                failed_command(2),
                unrelated_command,
                failed_command(4),
                read,
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind repair context workspace")
            .with_sandbox_backend(AgentAlwaysFailBackend),
    )
    .run(
        &AgentLoopInput::new("thread_context", "turn_context", "repair the fixture")
            .with_max_turns(6),
    );

    assert_eq!(result.status, AgentStatus::Failed, "result={result:?}");
    assert_eq!(result.error.as_deref(), Some("max turns exceeded"));
    assert!(result.pending_approvals.is_empty());
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    let plan_result = result
        .tool_results
        .iter()
        .find(|tool_result| tool_result.tool_name == "update_plan")
        .expect("verification plan result");
    let plan_payload = plan_result.to_message_payload();
    let verification = &plan_payload["content"]["verification"][0];
    assert_eq!(verification["action"]["command"], command);
    assert_eq!(verification["action"]["cwd"], ".");
    assert_eq!(verification["action"]["timeout_seconds"], 10);
    assert_eq!(verification["action"]["sandbox_mode"], "workspace_write");
    assert_eq!(verification["action"]["network_access"], "denied");
    assert_eq!(verification["action_scope_digest"], expected_scope_digest);
    let requests = seen_requests.lock().expect("repair context requests");
    assert!(requests.iter().any(|request| {
        request.messages.iter().any(|message| {
            message.role == ModelRole::Developer
                && message.content.contains("repair_context=")
                && message.content.contains("failed_requirement")
                && message.content.contains("affected_path")
                && message.content.contains("affected_symbol")
                && message.content.contains("workspace_revision")
                && message.content.contains("previous_action")
                && message.content.contains("previous_result")
                && message.content.contains("required_verification_action")
                && message.content.contains("additional_verification_actions")
                && message
                    .content
                    .contains("\"remaining_verification_action_count\":2")
                && message.content.contains("\"verification_actions_truncated\":false")
                && message.content.contains(&command)
                && message.content.contains(&additional_command)
                && message.content.contains("\"timeout_seconds\":10")
                && message.content.contains("\"command_tool_input\"")
                && message.content.contains("\"enforced_policy_context\"")
                && message
                    .content
                    .contains("\"submit_only_command_tool_input\":true")
                && message
                    .content
                    .contains("\"next_action\":\"execute_command_tool_input_exactly\"")
                && message
                    .content
                    .contains("\"replan_or_mutate_before_execution\":false")
                && message
                    .content
                    .contains("\"sandbox_mode\":\"workspace_write\"")
                && message.content.contains("\"network_access\":\"denied\"")
                && message.content.contains(&expected_scope_digest)
                && message.content.contains(&expected_additional_scope_digest)
                && message.content.contains("\"remaining_success_count\":1")
                && message
                    .content
                    .contains("submit only its command_tool_input exactly as the next action")
                && message
                    .content
                    .contains("ordered future actions, not permission to batch exclusive command calls")
                && message
                    .content
                    .contains("Only when an exact command executes and fails should you choose a materially different repair strategy")
        })
    }));
    let repair_message = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .find(|message| {
            message.role == ModelRole::Developer
                && message.content.contains("additional_verification_actions")
                && message.content.contains(&command)
                && message.content.contains(&additional_command)
        })
        .expect("repair context with required and additional actions");
    let repair_context: serde_json::Value = serde_json::from_str(
        repair_message
            .content
            .split_once(" repair_context=")
            .expect("repair context marker")
            .1,
    )
    .expect("valid repair context JSON");
    assert_eq!(
        repair_context["required_verification_action"]["command_tool_input"]["command"],
        command
    );
    assert_eq!(
        repair_context["additional_verification_actions"][0]["command_tool_input"]["command"],
        additional_command
    );
    assert!(!requests.iter().any(|request| {
        request.messages.iter().any(|message| {
            message
                .content
                .contains("\"required_verification_action\":{\"command\":")
        })
    }));
    let serialized = serde_json::to_string(&result).expect("serialize repair context result");
    assert!(!serialized.contains("raw_arguments"));
}

#[test]
fn pre_plan_command_failure_commits_after_mutation_bound_verification() {
    let workspace = tempfile::tempdir().expect("repair workspace");
    let fixture_name = "pre_plan_command_repair.txt";
    std::fs::write(workspace.path().join(fixture_name), "before").expect("write repair fixture");
    let command = test_command_script("repair verification");

    let mut failed_command = ModelTurnResponse::completed(
        "model_request_turn_pre_plan_repair_0",
        "response_pre_plan_0",
        "",
    );
    failed_command.tool_calls.push(tool_call(
        "command_pre_plan_failure",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let mut mutation = ModelTurnResponse::completed(
        "model_request_turn_pre_plan_repair_1",
        "response_pre_plan_1",
        "",
    );
    mutation.tool_calls.push(tool_call(
        "edit_pre_plan_repair",
        "edit",
        serde_json::json!({
            "path": fixture_name,
            "expected": "before",
            "replacement": "after"
        }),
    ));
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_pre_plan_repair_2",
        "response_pre_plan_2",
        "",
    );
    plan.tool_calls.push(tool_call(
        "plan_pre_plan_repair",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "repair and verify", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": format!("changed {fixture_name}"),
                "affected_path": fixture_name,
                "affected_symbol": fixture_name,
                "current_gap": "the repair has not been revision-bound verified",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut verification = ModelTurnResponse::completed(
        "model_request_turn_pre_plan_repair_3",
        "response_pre_plan_3",
        "",
    );
    verification.tool_calls.push(tool_call(
        "command_pre_plan_pass",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_pre_plan_repair_4",
        "response_pre_plan_4",
        "done",
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![failed_command, mutation, plan, verification, final_response],
            seen_requests,
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind repair workspace")
            .with_sandbox_backend(AgentFailThenSucceedBackend {
                calls: AtomicUsize::new(0),
            }),
    )
    .run(&AgentLoopInput::new(
        "thread_pre_plan_repair",
        "turn_pre_plan_repair",
        "repair after the failed command",
    ));

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
}

#[test]
fn changed_same_tool_retry_commits_after_revision_bound_verification() {
    let workspace = tempfile::tempdir().expect("same-tool repair workspace");
    let fixture_name = "same_tool_repair.txt";
    std::fs::write(workspace.path().join(fixture_name), "before").expect("write repair fixture");
    let command = test_command_script("same tool repair verification");

    let edit_response = |request: &str, response: &str, call: &str, expected: &str| {
        let mut model_response = ModelTurnResponse::completed(request, response, "");
        model_response.tool_calls.push(tool_call(
            call,
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": expected,
                "replacement": "after"
            }),
        ));
        model_response
    };
    let mut plan = ModelTurnResponse::completed(
        "model_request_turn_same_tool_repair_2",
        "response_same_tool_2",
        "",
    );
    plan.tool_calls.push(tool_call(
        "plan_same_tool_repair",
        "update_plan",
        serde_json::json!({
            "steps": [{"step": "retry edit and verify", "status": "completed"}],
            "verification": [{
                "risk": "general_mutation",
                "evidence": format!("changed {fixture_name}"),
                "affected_path": fixture_name,
                "affected_symbol": fixture_name,
                "current_gap": "the successful edit retry has not been verified",
                "action": {
                    "command": command,
                    "cwd": ".",
                    "timeout_seconds": 5,
                    "sandbox_mode": "workspace_write",
                    "network_access": "denied"
                },
            }]
        }),
    ));
    let mut verification = ModelTurnResponse::completed(
        "model_request_turn_same_tool_repair_3",
        "response_same_tool_3",
        "",
    );
    verification.tool_calls.push(tool_call(
        "command_same_tool_pass",
        "command",
        serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_same_tool_repair_4",
        "response_same_tool_4",
        "done",
    );
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit_response(
                    "model_request_turn_same_tool_repair_0",
                    "response_same_tool_0",
                    "edit_same_tool_failure",
                    "missing",
                ),
                edit_response(
                    "model_request_turn_same_tool_repair_1",
                    "response_same_tool_1",
                    "edit_same_tool_success",
                    "before",
                ),
                plan,
                verification,
                final_response,
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind same-tool repair workspace")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&AgentLoopInput::new(
        "thread_same_tool_repair",
        "turn_same_tool_repair",
        "retry the edit and verify it",
    ));

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
}

#[test]
fn repair_mutation_checkpoint_commits_the_new_revision_before_replanning() {
    let workspace = tempfile::tempdir().expect("repair checkpoint workspace");
    let fixture_name = "repair_checkpoint.txt";
    std::fs::write(workspace.path().join(fixture_name), "v0")
        .expect("write repair checkpoint fixture");
    let command = test_command_script("repair checkpoint verification");

    let edit_response = |turn: u32, call_id: &str, expected: &str, replacement: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_repair_checkpoint_{turn}"),
            format!("response_checkpoint_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": expected,
                "replacement": replacement,
            }),
        ));
        response
    };
    let plan_response = |turn: u32, call_id: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_repair_checkpoint_{turn}"),
            format!("response_checkpoint_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "repair and verify the fixture", "status": "completed"}],
                "verification": [{
                    "risk": "general_mutation",
                    "evidence": format!("changed {fixture_name}"),
                    "affected_path": fixture_name,
                    "affected_symbol": fixture_name,
                    "current_gap": "the current revision still needs verification",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        response
    };
    let command_response = |turn: u32, call_id: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_repair_checkpoint_{turn}"),
            format!("response_checkpoint_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            call_id,
            "command",
            serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
        ));
        response
    };
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_repair_checkpoint_7",
        "response_checkpoint_7",
        "done",
    );
    let policy = allow_read_execute_policy().with_rule(
        PermissionRule::new(
            "allow_write",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Write),
    );
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit_response(0, "edit_revision_1", "v0", "v1"),
                command_response(1, "command_before_plan"),
                plan_response(2, "plan_revision_1"),
                command_response(3, "command_revision_1"),
                edit_response(4, "edit_revision_2", "v1", "v2"),
                plan_response(5, "plan_revision_2"),
                command_response(6, "command_revision_2"),
                final_response,
            ],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind repair checkpoint workspace")
            .with_sandbox_backend(AgentFailThenSucceedBackend {
                calls: AtomicUsize::new(0),
            }),
    );
    let input = AgentLoopInput::new(
        "thread_repair_checkpoint",
        "turn_repair_checkpoint",
        "repair and verify the fixture",
    )
    .with_max_turns(8);
    let mut checkpoint_events = Vec::new();
    let result =
        agent_loop.run_with_events_and_checkpoints(&input, &mut |_event| Ok(()), &mut |event| {
            checkpoint_events.push(event);
            Ok(())
        });

    assert_eq!(result.status, AgentStatus::Completed, "result={result:?}");
    assert!(!checkpoint_events.iter().any(|event| {
        matches!(
            &event.phase,
            TurnCheckpointPhase::ToolResultsCommitted { tool_call_ids }
                if tool_call_ids == &["command_before_plan".to_string()]
        )
    }));
    assert!(result.tool_results.iter().any(|result| {
        result.tool_call_id == "command_before_plan"
            && result.error_code.as_deref() == Some("tool_not_visible")
    }));
    let second_mutation = checkpoint_events
        .iter()
        .find(|event| {
            matches!(
                &event.phase,
                TurnCheckpointPhase::ToolResultsCommitted { tool_call_ids }
                    if tool_call_ids == &["edit_revision_2".to_string()]
            )
        })
        .expect("second mutation checkpoint");
    let checkpoint = second_mutation
        .checkpoint
        .encode()
        .expect("second mutation checkpoint encodes");
    assert_eq!(checkpoint["completion"]["workspace_revision"], 2);
    assert_eq!(checkpoint["repair_plan"]["plan"]["required_revision"], 2);
    assert_eq!(checkpoint["repair_plan"]["plan"]["required_check_count"], 0);
    let edit_result = checkpoint["tool_result_occurrences"]
        .as_array()
        .expect("checkpoint tool results")
        .iter()
        .find(|occurrence| occurrence["result"]["tool_call_id"] == "edit_revision_2")
        .expect("second edit tool result");
    assert_eq!(
        edit_result["workspace_observation"],
        serde_json::json!({"revision": 2, "mutation": "changed"})
    );
    assert!(!checkpoint_events.iter().any(|event| {
        matches!(
            event.phase,
            TurnCheckpointPhase::ToolResultsCommitted { .. }
        ) && event.checkpoint.encode().is_err()
    }));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(fixture_name)).expect("read fixture"),
        "v2"
    );
}

#[test]
fn repair_budget_survives_mutation_replan_and_checkpoint_resume() {
    let workspace = tempfile::tempdir().expect("repair budget workspace");
    let fixture_name = "repair_budget.txt";
    std::fs::write(workspace.path().join(fixture_name), "v0").expect("write repair fixture");
    let command = test_command_script("failure");

    let edit_response = |turn_index: u32, expected: &str, replacement: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_budget_{turn_index}"),
            format!("response_edit_{turn_index}"),
            "",
        );
        let call_id = format!("edit_{turn_index}");
        response.tool_calls.push(tool_call(
            &call_id,
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": expected,
                "replacement": replacement
            }),
        ));
        response
    };
    let plan_response = |turn_index: u32| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_budget_{turn_index}"),
            format!("response_plan_{turn_index}"),
            "",
        );
        let call_id = format!("plan_{turn_index}");
        response.tool_calls.push(tool_call(
            &call_id,
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "repair and verify the bounded fixture", "status": "completed"}],
                "verification": [{
                    "risk": "general_mutation",
                    "evidence": format!("changed {fixture_name}"),
                    "affected_path": fixture_name,
                    "affected_symbol": fixture_name,
                    "current_gap": "command_exit_nonzero was observed; rerun after repair",
                    "action": {
                        "command": command,
                        "cwd": ".",
                        "timeout_seconds": 5,
                        "sandbox_mode": "workspace_write",
                        "network_access": "denied"
                    },
                }]
            }),
        ));
        response
    };
    let command_response = |turn_index: u32| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_budget_{turn_index}"),
            format!("response_command_{turn_index}"),
            "",
        );
        let call_id = format!("command_{turn_index}");
        response.tool_calls.push(tool_call(
            &call_id,
            "command",
            serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
        ));
        response
    };
    let read_response = |turn_index: u32| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_budget_{turn_index}"),
            format!("response_read_{turn_index}"),
            "",
        );
        let call_id = format!("read_{turn_index}");
        response.tool_calls.push(tool_call(
            &call_id,
            "read",
            serde_json::json!({"path": fixture_name}),
        ));
        response
    };

    let responses = vec![
        edit_response(0, "v0", "v1"),
        plan_response(1),
        command_response(2),
        edit_response(3, "v1", "v2"),
        plan_response(4),
        command_response(5),
        read_response(6),
        read_response(7),
        edit_response(8, "v2", "v3"),
        plan_response(9),
        command_response(10),
        edit_response(11, "v3", "v4"),
        plan_response(12),
        command_response(13),
    ];
    let policy = allow_read_execute_policy()
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
                "ask_read",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Read)
            .for_resource(workspace_resource(fixture_name)),
        );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses,
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind repair budget workspace")
            .with_sandbox_backend(AgentAlwaysFailBackend),
    );
    let input = AgentLoopInput::new(
        "thread_budget",
        "turn_budget",
        "repair within the bounded budget",
    )
    .with_max_turns(14);

    let first_blocked = agent_loop.run(&input);
    assert_eq!(
        first_blocked.status,
        AgentStatus::Blocked,
        "result={first_blocked:?}"
    );
    let first_pending = pending_approval(&first_blocked);
    let first_checkpoint = first_pending.encode_checkpoint().expect("first checkpoint");
    assert_eq!(first_checkpoint["checkpoint_version"], 3);
    assert_eq!(
        first_checkpoint["repair_attempts"], 1,
        "checkpoint={first_checkpoint}"
    );
    assert_eq!(first_checkpoint["repair_plan"]["plan"]["attempt"], 2);
    assert_eq!(
        first_checkpoint["recovery_metrics"]["repair_attempt_count"],
        1
    );
    let mut missing_change_summary = first_checkpoint.clone();
    missing_change_summary
        .as_object_mut()
        .expect("checkpoint object")
        .remove("verification_change");
    let missing_change = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &missing_change_summary,
    );
    assert_eq!(
        missing_change.expect_err("planned mutation must retain its change summary"),
        "approval checkpoint workspace change summary is missing"
    );
    let mut unbound_plan = first_checkpoint.clone();
    unbound_plan["verification_plan"]["revision"] = serde_json::Value::Null;
    let unbound_plan = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &unbound_plan,
    );
    assert_eq!(
        unbound_plan.expect_err("mutated checkpoint plan must remain revision bound"),
        "approval checkpoint verification plan revision binding is missing"
    );
    let mut parent_path = first_checkpoint.clone();
    parent_path["verification_change"]["changed_paths"] = serde_json::json!(["../outside"]);
    let parent_path = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &parent_path,
    );
    assert_eq!(
        parent_path.expect_err("parent path must fail closed"),
        "approval checkpoint verification change summary is invalid"
    );
    let mut rebound_path = first_checkpoint.clone();
    rebound_path["verification_change"]["changed_paths"] = serde_json::json!(["different.txt"]);
    let rebound_path = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &rebound_path,
    );
    assert_eq!(
        rebound_path.expect_err("valid-looking path cannot replace producer evidence"),
        "approval checkpoint verification change summary is not bound to its tool occurrence"
    );
    let mut rebound_digest = first_checkpoint.clone();
    rebound_digest["verification_change"]["diff_digest"] =
        serde_json::json!(format!("sha256:{}", "0".repeat(64)));
    let rebound_digest = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &rebound_digest,
    );
    assert_eq!(
        rebound_digest.expect_err("valid-looking digest cannot replace producer evidence"),
        "approval checkpoint verification change summary is not bound to its tool occurrence"
    );
    let mut reset_ledger = first_checkpoint.clone();
    reset_ledger["repair_attempts"] = serde_json::json!(0);
    reset_ledger["repair_plan"] = serde_json::Value::Null;
    let reset = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &reset_ledger,
    );
    assert_eq!(
        reset.expect_err("repair attempt ledger cannot be reset"),
        "approval checkpoint repair attempt metrics are inconsistent"
    );
    let mut coordinated_reset = first_checkpoint.clone();
    coordinated_reset["repair_attempts"] = serde_json::json!(0);
    coordinated_reset["recovery_metrics"]["repair_attempt_count"] = serde_json::json!(0);
    coordinated_reset["repair_plan"] = serde_json::Value::Null;
    let coordinated_reset = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &coordinated_reset,
    );
    assert_eq!(
        coordinated_reset.expect_err("unresolved failures require a coordinated repair state"),
        "approval checkpoint repair cycle ledger is inconsistent"
    );
    let mut active_ledger_reset = first_checkpoint.clone();
    active_ledger_reset["repair_attempts"] = serde_json::json!(0);
    active_ledger_reset["recovery_metrics"]["repair_attempt_count"] = serde_json::json!(0);
    active_ledger_reset["repair_plan"]["plan"]["attempt"] = serde_json::json!(1);
    let active_ledger_reset = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &active_ledger_reset,
    );
    assert_eq!(
        active_ledger_reset.expect_err("active repair ledger cannot be rolled back"),
        "approval checkpoint repair cycle ledger is inconsistent"
    );

    let first_resumed_input = input.clone().with_approval_grant(ApprovalGrant::allow(
        first_pending.pending_tool_call().request_id.clone(),
        first_pending.pending_tool_call().tool_name.clone(),
        first_pending.pending_tool_call().resources.clone(),
    ));
    let first_restored = PendingApprovalOccurrence::from_checkpoint_payload(
        first_pending.request().clone(),
        &first_checkpoint,
    )
    .expect("restore first checkpoint");
    let second_blocked = agent_loop.resume_pending_approval(&first_resumed_input, &first_restored);
    assert_eq!(
        second_blocked.status,
        AgentStatus::Blocked,
        "result={second_blocked:?} raw={} last={}",
        first_checkpoint["raw_arguments"],
        first_checkpoint["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
    );
    let second_pending = pending_approval(&second_blocked);
    let second_checkpoint = second_pending
        .encode_checkpoint()
        .expect("second checkpoint");
    assert_eq!(second_checkpoint["repair_attempts"], 1);
    assert_eq!(second_checkpoint["repair_plan"]["plan"]["attempt"], 2);

    let second_resumed_input = first_resumed_input.with_approval_grant(ApprovalGrant::allow(
        second_pending.pending_tool_call().request_id.clone(),
        second_pending.pending_tool_call().tool_name.clone(),
        second_pending.pending_tool_call().resources.clone(),
    ));
    let second_restored = PendingApprovalOccurrence::from_checkpoint_payload(
        second_pending.request().clone(),
        &second_checkpoint,
    )
    .expect("restore second checkpoint");
    let mut events = Vec::new();
    let exhausted = agent_loop.resume_pending_approval_with_events(
        &second_resumed_input,
        &second_restored,
        &mut |event| {
            events.push(event);
            Ok(())
        },
    );

    assert_eq!(
        exhausted.status,
        AgentStatus::Failed,
        "result={exhausted:?}"
    );
    assert!(
        exhausted
            .error
            .as_deref()
            .is_some_and(|error| error.contains("repair planning budget exhausted"))
    );
    assert_eq!(exhausted.recovery_metrics.repair_attempt_count, 3);
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::RepairPlanning(repair))
            if repair.attempt > 3
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(fixture_name)).unwrap(),
        "v4"
    );
    assert_eq!(seen_requests.lock().expect("seen requests").len(), 14);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentLoopEvent::Observation(AgentObservation::RepairPlanning(repair))
            if repair.reason == AgentRepairReason::VerificationFailed
                && repair.attempt == 3
                && matches!(repair.lifecycle, OccurrenceLifecycle::Finished {
                    status: RepairPlanningStatus::Exhausted,
                    ..
                })
    )));
    let requests = seen_requests.lock().expect("seen requests");
    let repair_context = requests
        .iter()
        .flat_map(|request| request.messages.iter())
        .find(|message| {
            message.role == ModelRole::Developer && message.content.contains("repair_context=")
        })
        .expect("mutation-bound repair context");
    assert!(repair_context.content.contains(fixture_name));
    assert!(repair_context.content.contains("diff_digest"));
    assert!(repair_context.content.chars().count() < 10_000);
    assert!(
        !repair_context
            .content
            .contains("\"previous_action\":\"command\"")
    );
}

#[test]
fn repair_cycle_requires_matching_scope_and_all_revision_checks() {
    let workspace = tempfile::tempdir().expect("multi-check workspace");
    let fixture_name = "repair_multi_check.txt";
    std::fs::write(workspace.path().join(fixture_name), "v0").expect("write repair fixture");
    let first_command = test_command_script("failure");
    let second_command = test_command_script("success_a");
    let third_command = test_command_script("success_b");

    let edit_response = |turn: u32, expected: &str, replacement: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_multi_check_{turn}"),
            format!("response_multi_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            &format!("edit_multi_{turn}"),
            "edit",
            serde_json::json!({
                "path": fixture_name,
                "expected": expected,
                "replacement": replacement,
            }),
        ));
        response
    };
    let plan_response = |turn: u32, first: &str, second: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_multi_check_{turn}"),
            format!("response_multi_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            &format!("plan_multi_{turn}"),
            "update_plan",
            serde_json::json!({
                "steps": [{"step": "repair and run both checks", "status": "completed"}],
                "verification": [
                    {
                        "risk": "general_mutation",
                        "evidence": format!("changed {fixture_name}"),
                        "affected_path": fixture_name,
                        "affected_symbol": "repair_multi_check::first",
                        "current_gap": "command_exit_nonzero remains unresolved for the first check",
                        "action": {
                            "command": first,
                            "cwd": ".",
                            "timeout_seconds": 5,
                            "sandbox_mode": "workspace_write",
                            "network_access": "denied"
                        },
                    },
                    {
                        "risk": "general_mutation",
                        "evidence": format!("changed {fixture_name}"),
                        "affected_path": fixture_name,
                        "affected_symbol": "repair_multi_check::second",
                        "current_gap": "command_exit_nonzero requires rerunning the second check",
                        "action": {
                            "command": second,
                            "cwd": ".",
                            "timeout_seconds": 5,
                            "sandbox_mode": "workspace_write",
                            "network_access": "denied"
                        },
                    }
                ]
            }),
        ));
        response
    };
    let command_response = |turn: u32, command: &str| {
        let mut response = ModelTurnResponse::completed(
            format!("model_request_turn_multi_check_{turn}"),
            format!("response_multi_{turn}"),
            "",
        );
        response.tool_calls.push(tool_call(
            &format!("command_multi_{turn}"),
            "command",
            serde_json::json!({"command": command, "cwd": ".", "timeout_seconds": 5}),
        ));
        response
    };
    let mut read_response =
        ModelTurnResponse::completed("model_request_turn_multi_check_6", "response_multi_6", "");
    read_response.tool_calls.push(tool_call(
        "read_multi",
        "read",
        serde_json::json!({"path": fixture_name}),
    ));
    let final_response = ModelTurnResponse::completed(
        "model_request_turn_multi_check_8",
        "response_multi_8",
        "done",
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let policy = allow_read_execute_policy()
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
                "ask_read",
                SettingsScope::Project,
                PermissionDecisionOutcome::Ask,
            )
            .for_operation(PermissionOperation::Read)
            .for_resource(workspace_resource(fixture_name)),
        );
    let agent_loop = AgentLoop::new(
        StaticProvider {
            responses: vec![
                edit_response(0, "v0", "v1"),
                plan_response(1, &first_command, &third_command),
                command_response(2, &first_command),
                edit_response(3, "v1", "v2"),
                plan_response(4, &second_command, &third_command),
                command_response(5, &second_command),
                read_response,
                command_response(7, &third_command),
                final_response,
            ],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(true),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind multi-check workspace")
            .with_sandbox_backend(AgentFailThenSucceedBackend {
                calls: AtomicUsize::new(0),
            }),
    );
    let input = AgentLoopInput::new(
        "thread_multi_check",
        "turn_multi_check",
        "repair and run both checks",
    )
    .with_max_turns(9);

    let result = agent_loop.run(&input);
    assert_eq!(result.status, AgentStatus::Completed, "{result:?}");
    assert_eq!(result.recovery_metrics.repair_attempt_count, 1);
    assert_eq!(result.recovery_metrics.invalid_tool_call_count, 1);
    assert_eq!(result.verification.satisfied_command_count, 2);
    assert!(result.pending_approvals.is_empty());
    assert!(result.tool_results.iter().any(|tool_result| {
        tool_result.tool_call_id == "read_multi"
            && tool_result.error_code.as_deref() == Some("tool_not_visible")
    }));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(fixture_name)).expect("read result"),
        "v2"
    );
}

#[test]
fn malformed_final_review_retries_within_the_model_turn_budget() {
    let workspace = tempfile::tempdir().expect("workspace");
    let command = test_command_script("verify");
    let mut verification =
        ModelTurnResponse::completed("model_request_turn_review_retry_0", "response_verify", "");
    verification.tool_calls.push(tool_call(
        "verify_call",
        "command",
        serde_json::json!({
            "command": command,
            "cwd": ".",
            "timeout_seconds": 5
        }),
    ));
    let malformed = ModelTurnResponse::completed(
        "model_request_turn_review_retry_1",
        "response_malformed_review",
        r#"{"verdict":"accept"}"#,
    );
    let valid = ModelTurnResponse::completed(
        "model_request_turn_review_retry_2",
        "response_valid_review",
        "done",
    );
    let exhausted_responses = vec![verification.clone(), malformed.clone()];
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut events = Vec::new();
    let mut checkpoints = Vec::new();

    let result = AgentLoop::new(
        StaticProvider {
            responses: vec![verification, malformed, valid],
            seen_requests: Arc::clone(&seen_requests),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run_with_events_and_checkpoints(
        &AgentLoopInput::new("thread_review_retry", "turn_review_retry", "verify")
            .with_max_turns(3)
            .with_verification_commands([verification_command(command.clone(), 1)]),
        &mut |event| {
            events.push(event);
            Ok(())
        },
        &mut |checkpoint| {
            checkpoints.push(checkpoint);
            Ok(())
        },
    );

    assert_eq!(result.status, AgentStatus::Completed, "{result:?}");
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(result.model_turns, 3);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 1);
    let final_review_statuses = events
        .iter()
        .filter_map(|event| match event {
            AgentLoopEvent::Observation(AgentObservation::FinalReview(value)) => {
                match value.lifecycle {
                    OccurrenceLifecycle::Finished { status, .. } => Some(status),
                    OccurrenceLifecycle::Started { .. } | OccurrenceLifecycle::Suspended { .. } => {
                        None
                    }
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        final_review_statuses,
        [FinalReviewStatus::Failed, FinalReviewStatus::Succeeded]
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].tool_choice.mode, ToolChoiceMode::None);
    assert_eq!(requests[2].tool_choice.mode, ToolChoiceMode::None);
    assert!(requests[2].messages.iter().any(|message| {
        message.role == ModelRole::Developer
            && message
                .content
                .contains("previous final review response was invalid")
    }));
    drop(requests);
    checkpoints
        .iter()
        .find(|event| {
            matches!(
                event.phase,
                TurnCheckpointPhase::BeforeModelRequest {
                    finalization_only: true
                }
            ) && event.checkpoint.encode().is_ok_and(|payload| {
                payload["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "assistant"
                            && message["content"] == r#"{"verdict":"accept"}"#
                    }) && messages.iter().any(|message| {
                        message["role"] == "developer"
                            && message["content"].as_str().is_some_and(|content| {
                                content.contains("previous final review response was invalid")
                            })
                    })
                })
            })
        })
        .expect("retry checkpoint preserves malformed assistant and correction");

    let exhausted = AgentLoop::new(
        StaticProvider {
            responses: exhausted_responses,
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            capabilities: ProviderProtocolContract::default(),
        },
        agent_tool_broker_for_test(false),
        allow_read_execute_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("rebind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(
        &AgentLoopInput::new("thread_review_retry", "turn_review_retry", "verify")
            .with_max_turns(1)
            .with_verification_commands([verification_command(command, 1)]),
    );
    assert_eq!(exhausted.status, AgentStatus::Failed, "{exhausted:?}");
    assert_eq!(
        exhausted.error.as_deref(),
        Some("final review response is not a strict typed JSON object")
    );
    assert_eq!(exhausted.model_turns, 2);
    assert_eq!(exhausted.tool_results.len(), 1);
    assert_eq!(exhausted.recovery_metrics.repair_attempt_count, 0);
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
        let verification_argv = test_command("verify");
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
                ..Default::default()
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
        verification
            .provider_attempt_metadata
            .as_mut()
            .expect("verification attempt metadata")
            .occurrences
            .push(provider_attempt_occurrence(
                41,
                "provider-verification",
                ProviderAttemptStatus::Ok,
            ));

        let final_response = match case {
            FinalizationCase::ProviderError => {
                let mut first = provider_attempt_occurrence(
                    88,
                    "provider-error-first",
                    ProviderAttemptStatus::Error,
                );
                first.error_category = Some(ModelErrorCategory::UnknownProviderError);
                first.error_stage = Some(singularity_model::ProviderErrorStage::RequestSend);
                first.diagnostic_code = Some("terminal_provider_failed".to_string());
                let mut retry = first.clone();
                retry.attempt_index = 89;
                retry.provider_name = "provider-error-retry".to_string();
                let metadata = ProviderAttemptMetadata {
                    attempt_count: 2,
                    retry_count: 1,
                    latency_ms: 40,
                    occurrences: vec![first, retry],
                };
                Err(ProviderError::from_model_error(
                    ModelError::new(
                        ModelErrorKind::UnknownProviderError,
                        "terminal provider failed",
                    )
                    .with_provider_diagnostic(
                        "terminal_provider_failed",
                        singularity_model::ProviderErrorStage::RequestSend,
                    ),
                )
                .with_provider_attempt_metadata(metadata))
            }
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
            WorkspaceTools::new(workspace.path())
                .expect("bind workspace tools")
                .with_sandbox_backend(AgentStrictBackend),
        )
        .with_cancellation_token(cancellation);

        let mut events = Vec::new();
        let result = result.run_with_events(
            &AgentLoopInput::new("thread_1", "turn_1", "verify")
                .with_max_turns(1)
                .with_verification_commands([verification_command(verification_argv.join(" "), 1)]),
            &mut |event| {
                events.push(event);
                Ok(())
            },
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
        let expected_final_review_status = match case {
            FinalizationCase::Cancelled => FinalReviewStatus::Cancelled,
            FinalizationCase::ProviderError
            | FinalizationCase::EmptyResponse
            | FinalizationCase::StructuredToolCall => FinalReviewStatus::Failed,
        };
        assert!(events.iter().any(|event| matches!(
            event,
            AgentLoopEvent::Observation(AgentObservation::FinalReview(value))
                if matches!(value.lifecycle, OccurrenceLifecycle::Finished { status, .. }
                    if status == expected_final_review_status)
        )));

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
                assert_eq!(
                    result
                        .provider_attempts
                        .occurrences
                        .iter()
                        .map(|occurrence| occurrence.attempt_index)
                        .collect::<Vec<_>>(),
                    [1, 2, 3]
                );
                assert_eq!(
                    result
                        .provider_attempts
                        .occurrences
                        .iter()
                        .map(|occurrence| occurrence.terminal_status)
                        .collect::<Vec<_>>(),
                    [
                        ProviderAttemptStatus::Ok,
                        ProviderAttemptStatus::Error,
                        ProviderAttemptStatus::Error
                    ]
                );
                assert_eq!(
                    result.provider_attempts.occurrences[1]
                        .diagnostic_code
                        .as_deref(),
                    Some("terminal_provider_failed")
                );
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
        .with_verification_commands([verification_command(test_command_script("unobserved"), 1)]);
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
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("evidence.txt"), "trusted").expect("write evidence");
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
    let mut successful_read =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_read", "");
    successful_read.tool_calls.push(tool_call(
        "call_read",
        "read",
        serde_json::json!({"path": "evidence.txt"}),
    ));
    let mut successful_command =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_3", "");
    successful_command.tool_calls.push(tool_call(
        "call_3",
        "command",
        serde_json::json!({"command": test_command_script("success"), "timeout_seconds": 5}),
    ));
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_4", "response_4", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let result = agent_loop_with_capabilities(
        vec![
            first_invalid,
            second_invalid,
            successful_read,
            successful_command,
            final_response,
        ],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    )
    .run(&AgentLoopInput::new("thread_1", "turn_1", "verify").with_max_turns(5));

    assert_eq!(result.status, AgentStatus::Completed);
    assert_eq!(result.recovery_metrics.invalid_tool_call_count, 2);
    assert_eq!(result.recovery_metrics.repeated_tool_call_count, 1);
    assert_eq!(result.recovery_metrics.repair_attempt_count, 0);
    assert_eq!(result.recovery_metrics.completion_rejection_count, 0);
    let requests = seen_requests.lock().expect("seen requests");
    for (request, call_id) in [(&requests[1], "call_1"), (&requests[2], "call_2")] {
        let assistant_call = request
            .messages
            .iter()
            .flat_map(|message| message.tool_calls.iter())
            .find(|call| call.tool_call_id == call_id)
            .expect("repeated invalid assistant call");
        assert_eq!(assistant_call.tool_name, "command");
        assert_eq!(assistant_call.arguments, serde_json::json!({}));
        assert_eq!(assistant_call.raw_arguments, "{}");
        let feedback = request
            .messages
            .iter()
            .find(|message| {
                message.role == ModelRole::Tool && message.tool_call_id.as_deref() == Some(call_id)
            })
            .expect("repeated invalid feedback");
        let payload: serde_json::Value =
            serde_json::from_str(&feedback.content).expect("repeated structured feedback");
        assert_eq!(payload["tool_call_id"], call_id);
        assert_eq!(payload["tool_name"], "command");
        for field in [
            "visible_tool_names",
            "rejection_kind",
            "name_projection",
            "correction",
            "placeholder_non_callable",
        ] {
            assert!(
                payload["content"].get(field).is_none(),
                "unexpected field {field}"
            );
        }
        assert!(
            request
                .tools
                .iter()
                .all(|tool| tool.name != "tool_rejected")
        );
    }
    assert!(requests[2].messages.iter().any(|message| {
        message.role == ModelRole::Developer
            && message
                .content
                .contains("same repairable tool failure recurred")
    }));
    assert!(requests[3].messages.iter().any(|message| {
        message.role == ModelRole::Developer && message.content.contains("repair_context=")
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
        occurrences: vec![provider_attempt_occurrence(
            101,
            "provider-plan",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
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
        occurrences: vec![
            provider_attempt_occurrence(202, "provider-edit-first", ProviderAttemptStatus::Ok),
            provider_attempt_occurrence(202, "provider-edit-retry", ProviderAttemptStatus::Error),
        ],
        ..Default::default()
    });
    let mut verify_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_3", "");
    verify_response.tool_calls.push(tool_call(
        "verify_call_1",
        "command",
        serde_json::json!({
            "command": test_command_script("success"),
            "cwd": ".",
            "timeout_seconds": 5
        }),
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
        occurrences: vec![provider_attempt_occurrence(
            303,
            "provider-verify",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    let mut final_response =
        ModelTurnResponse::completed("model_request_turn_1_4", "response_4", "done");
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
        occurrences: vec![provider_attempt_occurrence(
            404,
            "provider-final",
            ProviderAttemptStatus::Ok,
        )],
        ..Default::default()
    });
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let post_mutation_plan = workspace_verification_plan_response(
        "model_request_turn_1_2",
        "response_plan_after_edit",
        "plan_call_after_edit",
        "success",
    );
    let agent_loop = agent_loop_with_plan_capabilities(
        vec![
            plan_response,
            edit_response,
            post_mutation_plan,
            verify_response,
            final_response,
        ],
        allow_read_execute_policy(),
        Arc::clone(&seen_requests),
        ProviderProtocolContract::default(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(workspace.path())
            .expect("bind workspace tools")
            .with_sandbox_backend(AgentStrictBackend),
    );
    let input = AgentLoopInput::new("thread_1", "turn_1", "edit")
        .with_max_turns(4)
        .with_verification_commands([verification_command(test_command_script("success"), 1)]);
    let blocked = agent_loop.run(&input);

    assert_eq!(blocked.status, AgentStatus::Blocked);
    assert_eq!(blocked.plan_update_count, 1);
    assert_eq!(blocked.recovery_metrics, AgentRecoveryMetrics::default());
    assert_eq!(
        blocked
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| occurrence.attempt_index)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let pending = pending_approval(&blocked);
    let checkpoint = pending.encode_checkpoint().expect("approval checkpoint");
    assert_eq!(checkpoint["plan"]["steps"][0]["status"], "completed");
    assert_eq!(checkpoint["plan_update_count"], 1);
    assert_eq!(checkpoint["recovery_metrics"]["repair_attempt_count"], 0);
    assert_eq!(checkpoint["model_usage"]["total_tokens"], 33);
    assert_eq!(checkpoint["provider_attempts"]["attempt_count"], 3);
    assert_eq!(checkpoint["provider_attempts"]["retry_count"], 1);
    assert!(checkpoint["provider_attempts"].get("occurrences").is_none());
    assert!(checkpoint["seen_tool_call_fingerprints"].is_array());
    assert!(checkpoint["last_repair_failure"].is_null());

    let resumed_input = input.with_approval_grant(ApprovalGrant::allow(
        pending.pending_tool_call().request_id.clone(),
        pending.pending_tool_call().tool_name.clone(),
        pending.pending_tool_call().resources.clone(),
    ));
    let restored =
        PendingApprovalOccurrence::from_checkpoint_payload(pending.request().clone(), &checkpoint)
            .expect("approval checkpoint decode");
    let resumed = agent_loop.resume_pending_approval(&resumed_input, &restored);

    assert_eq!(resumed.status, AgentStatus::Completed, "{resumed:?}");
    assert_eq!(resumed.model_turns, 5);
    assert_eq!(resumed.model_turn_limit, 4);
    assert_eq!(resumed.plan_update_count, 2);
    assert_eq!(resumed.recovery_metrics, AgentRecoveryMetrics::default());
    assert_eq!(resumed.model_usage.input_tokens, 100);
    assert_eq!(resumed.model_usage.output_tokens, 10);
    assert_eq!(resumed.model_usage.total_tokens, 110);
    assert_eq!(resumed.provider_attempts.attempt_count, 5);
    assert_eq!(resumed.provider_attempts.retry_count, 1);
    assert_eq!(resumed.provider_attempts.latency_ms, 100);
    assert_eq!(
        resumed
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| occurrence.attempt_index)
            .collect::<Vec<_>>(),
        [4, 5]
    );
    assert_eq!(
        resumed
            .provider_attempts
            .occurrences
            .iter()
            .map(|occurrence| occurrence.provider_name.as_str())
            .collect::<Vec<_>>(),
        ["provider-verify", "provider-final"]
    );
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
    assert_eq!(requests.len(), 5);
    assert!(
        requests[..4]
            .iter()
            .all(|request| request.tool_choice.mode == ToolChoiceMode::Auto)
    );
    assert_eq!(requests[4].tool_choice.mode, ToolChoiceMode::None);
    assert!(requests[4].tools.is_empty());
}
