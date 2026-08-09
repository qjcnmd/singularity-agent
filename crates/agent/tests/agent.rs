//! AgentLoop public behavior regressions.

use std::sync::{Arc, Mutex};

use serde_json::json;
use singularity_agent::{
    AgentContinuation, AgentLoop, AgentLoopCallbacks, AgentLoopEvent, AgentLoopEventSinkError,
    AgentLoopInput, AgentStatus, TurnCheckpoint, TurnCheckpointEvent, TurnCheckpointPhase,
};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelToolCall, ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, Provider,
    ProviderProtocolContract, ProviderStreamEvent,
};
use singularity_policy::{
    PermissionDecisionOutcome, PermissionOperation, PermissionProfile, PermissionRule,
    PolicyEngine, SettingsScope,
};
use singularity_tools::{
    ToolBroker, ToolFailureKind, ToolRegistry, WorkspaceTools, workspace_tool_entries,
};

#[derive(Clone)]
struct FixtureProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl Provider for FixtureProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("provider requests lock");
        let response = self
            .responses
            .get(seen_requests.len())
            .or_else(|| self.responses.last())
            .expect("fixture response")
            .clone();
        seen_requests.push(request.clone());
        Ok(with_request_id(response, request))
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, singularity_model::ProviderError> {
        let response = self.complete(request, cancellation)?;
        if let Some(message) = response.assistant_message.as_ref() {
            on_event(ProviderStreamEvent::OutputTextDelta {
                delta: message.content.clone(),
            });
        }
        Ok(response)
    }
}

fn with_request_id(
    mut response: ModelTurnResponse,
    request: &ModelTurnRequest,
) -> ModelTurnResponse {
    response.request_id = request.request_id.clone();
    response
}

fn tool_broker() -> ToolBroker {
    let mut registry = ToolRegistry::default();
    for entry in workspace_tool_entries().into_iter().filter(|entry| {
        ["read", "list", "grep", "patch", "command"].contains(&entry.spec.name.as_str())
    }) {
        registry.register(entry).expect("register fixture tool");
    }
    ToolBroker::new(registry)
}

fn read_policy() -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write()).with_rule(
        PermissionRule::new(
            "allow_read",
            SettingsScope::Project,
            PermissionDecisionOutcome::Allow,
        )
        .for_operation(PermissionOperation::Read),
    )
}

fn approval_policy() -> PolicyEngine {
    PolicyEngine::new(PermissionProfile::workspace_write())
}

fn loop_with(
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    policy: PolicyEngine,
    cancellation: CancellationToken,
    _stream: bool,
) -> AgentLoop<FixtureProvider> {
    AgentLoop::new(
        FixtureProvider {
            responses,
            seen_requests,
        },
        tool_broker(),
        policy,
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR")).expect("fixture workspace tools"),
    )
    .with_cancellation_token(cancellation)
}

fn assistant_response(text: &str) -> ModelTurnResponse {
    ModelTurnResponse::completed("fixture_request", "fixture_response", text)
}

fn read_tool_response(path: &str) -> ModelTurnResponse {
    let mut response = assistant_response("reading");
    response.tool_calls.push(ModelToolCall {
        tool_call_id: "read_call".to_string(),
        tool_name: singularity_tools::READ_TOOL.to_string(),
        arguments: json!({"path": path}),
        raw_arguments: json!({"path": path}).to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    response
}

#[test]
fn natural_stop_commits_one_streamed_assistant_response() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![assistant_response("done")],
        Arc::clone(&seen_requests),
        read_policy(),
        CancellationToken::new(),
        true,
    );
    let mut events = Vec::new();
    let mut checkpoints = Vec::new();
    let mut on_event = |event: AgentLoopEvent| -> Result<(), AgentLoopEventSinkError> {
        events.push(event);
        Ok(())
    };
    let mut on_checkpoint = |event: TurnCheckpointEvent| -> Result<(), AgentLoopEventSinkError> {
        checkpoints.push(event);
        Ok(())
    };

    let result = loop_.run(
        &AgentLoopInput::new("thread_natural", "turn_natural", "say hello"),
        AgentLoopCallbacks::events_and_checkpoints(&mut on_event, &mut on_checkpoint),
    );

    assert_eq!(
        result.status,
        AgentStatus::Completed,
        "error: {:?}",
        result.error
    );
    assert_eq!(result.final_answer.as_deref(), Some("done"));
    assert_eq!(seen_requests.lock().expect("requests").len(), 1);
    assert!(events.iter().any(|event| {
        matches!(event, AgentLoopEvent::FinalTextDelta { delta } if delta == "done")
    }));
    assert!(
        checkpoints
            .iter()
            .any(|event| event.phase == TurnCheckpointPhase::ModelResponseCommitted)
    );
}

#[test]
fn recoverable_typed_tool_failure_reaches_the_next_model_turn() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![
            read_tool_response("missing-file-for-agent-test"),
            assistant_response("recovered"),
        ],
        Arc::clone(&seen_requests),
        read_policy(),
        CancellationToken::new(),
        false,
    );
    let result = loop_.run(
        &AgentLoopInput::new("thread_failure", "turn_failure", "inspect"),
        AgentLoopCallbacks::none(),
    );

    assert_eq!(
        result.status,
        AgentStatus::Completed,
        "error: {:?}",
        result.error
    );
    assert_eq!(result.final_answer.as_deref(), Some("recovered"));
    assert_eq!(seen_requests.lock().expect("requests").len(), 2);
    let failure = result.tool_results.first().expect("typed failure");
    assert!(!failure.ok);
    assert!(matches!(
        failure.failure_kind,
        Some(ToolFailureKind::Execution)
            | Some(ToolFailureKind::WorkspaceBoundary)
            | Some(ToolFailureKind::Input)
    ));
}

#[test]
fn approval_blocks_before_tool_execution() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![read_tool_response("README.md")],
        Arc::clone(&seen_requests),
        approval_policy(),
        CancellationToken::new(),
        false,
    );
    let result = loop_.run(
        &AgentLoopInput::new("thread_approval", "turn_approval", "run command"),
        AgentLoopCallbacks::none(),
    );

    assert_eq!(
        result.status,
        AgentStatus::Blocked,
        "error: {:?}",
        result.error
    );
    assert_eq!(result.pending_approvals.len(), 1);
    assert_eq!(result.tool_results.len(), 1);
    assert_eq!(
        result.tool_results[0].failure_kind,
        Some(ToolFailureKind::Approval)
    );
    assert_eq!(seen_requests.lock().expect("requests").len(), 1);
}

#[test]
fn cancellation_and_max_turns_are_terminal_without_a_repair_request() {
    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    let cancelled_seen = Arc::new(Mutex::new(Vec::new()));
    let cancelled_loop = loop_with(
        vec![assistant_response("must not run")],
        Arc::clone(&cancelled_seen),
        read_policy(),
        cancelled_token,
        false,
    );
    let cancelled = cancelled_loop.run(
        &AgentLoopInput::new("thread_cancel", "turn_cancel", "cancel"),
        AgentLoopCallbacks::none(),
    );
    assert_eq!(cancelled.status, AgentStatus::Cancelled);
    assert!(cancelled_seen.lock().expect("requests").is_empty());

    let max_turn_seen = Arc::new(Mutex::new(Vec::new()));
    let max_turn_loop = loop_with(
        vec![read_tool_response("missing-file-for-max-turn-test")],
        Arc::clone(&max_turn_seen),
        read_policy(),
        CancellationToken::new(),
        false,
    );
    let max_turn = max_turn_loop.run(
        &AgentLoopInput::new("thread_max", "turn_max", "inspect").with_max_turns(1),
        AgentLoopCallbacks::none(),
    );
    assert_eq!(
        max_turn.status,
        AgentStatus::Failed,
        "error: {:?}",
        max_turn.error
    );
    assert!(
        max_turn
            .error
            .as_deref()
            .is_some_and(|error| error.contains("max turns"))
    );
    assert_eq!(max_turn_seen.lock().expect("requests").len(), 1);
}

#[test]
fn response_checkpoint_roundtrip_keeps_occurrences_and_has_no_quality_gate_state() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![assistant_response("checkpointed")],
        Arc::clone(&seen_requests),
        read_policy(),
        CancellationToken::new(),
        false,
    );
    let mut committed = None;
    let mut on_checkpoint = |event: TurnCheckpointEvent| -> Result<(), AgentLoopEventSinkError> {
        if event.phase == TurnCheckpointPhase::ModelResponseCommitted {
            committed = Some(event.checkpoint);
        }
        Ok(())
    };
    let result = loop_.run(
        &AgentLoopInput::new("thread_checkpoint", "turn_checkpoint", "say hello"),
        AgentLoopCallbacks {
            on_event: None,
            on_checkpoint: Some(&mut on_checkpoint),
        },
    );
    assert_eq!(
        result.status,
        AgentStatus::Completed,
        "error: {:?}",
        result.error
    );
    let checkpoint = committed.expect("response checkpoint");
    assert_eq!(checkpoint.checkpoint_version(), 7);
    let encoded = checkpoint.encode().expect("encode checkpoint");
    assert!(encoded.get("completion").is_none());
    assert!(encoded.get("repair_state").is_none());
    let decoded = TurnCheckpoint::decode(&encoded).expect("decode checkpoint");
    assert_eq!(decoded.checkpoint_version(), 7);
    assert_eq!(decoded.model_turns(), 1);
    assert!(decoded.pending_tool_calls().is_empty());
}

#[test]
fn resume_dispatch_accepts_turn_continuation_without_replaying_completed_tool_calls() {
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![assistant_response("first"), assistant_response("second")],
        Arc::clone(&seen_requests),
        read_policy(),
        CancellationToken::new(),
        false,
    );
    let checkpoint = loop_
        .initial_turn_checkpoint(&AgentLoopInput::new(
            "thread_resume",
            "turn_resume",
            "start",
        ))
        .expect("initial checkpoint");
    let result = loop_.resume(
        &AgentLoopInput::new("thread_resume", "turn_resume", "start"),
        AgentContinuation::Turn(&checkpoint),
        AgentLoopCallbacks::none(),
    );
    assert_eq!(
        result.status,
        AgentStatus::Completed,
        "error: {:?}",
        result.error
    );
    assert_eq!(seen_requests.lock().expect("requests").len(), 1);
}
