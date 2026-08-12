//! AgentLoop public behavior regressions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::json;
use singularity_agent::{
    AgentContinuation, AgentLoop, AgentLoopCallbacks, AgentLoopEvent, AgentLoopEventSinkError,
    AgentLoopInput, AgentStatus, PendingApprovalOccurrence, TurnCheckpoint, TurnCheckpointEvent,
    TurnCheckpointPhase,
};
use singularity_core::CancellationToken;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelErrorCategory, ModelToolCall,
    ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, OpenAiProvider,
    OpenAiProviderConfig, Provider, ProviderConfigSource, ProviderProtocolContract, ProviderStreamEvent,
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

/// Use the real OpenAI adapter while keeping this regression deterministic and non-streaming.
/// The adapter still owns response parsing/validation; the wrapper only exposes the requests the
/// AgentLoop sees and lets the default stream fallback call `complete`.
#[derive(Clone)]
struct AdapterFixtureProvider {
    provider: OpenAiProvider,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl Provider for AdapterFixtureProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.provider.protocol_contract()
    }

    fn negotiate_tool_capabilities(
        &self,
        model_preferences: &singularity_model::ModelPreferences,
        cancellation: &CancellationToken,
    ) -> Result<singularity_model::ProviderProtocolNegotiation, singularity_model::ProviderError>
    {
        self.provider
            .negotiate_tool_capabilities(model_preferences, cancellation)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderError> {
        self.seen_requests
            .lock()
            .expect("adapter requests lock")
            .push(request.clone());
        self.provider.complete(request, cancellation)
    }
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

fn adapter_test_config(base_url: String) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url,
        api_key: "sk-test".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

fn adapter_response_server(
    endpoint: &'static str,
    responses: Vec<String>,
) -> (String, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind adapter fixture provider");
    listener
        .set_nonblocking(true)
        .expect("adapter listener nonblocking");
    let address = listener
        .local_addr()
        .expect("adapter fixture provider address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        // The provider runs its fixed capability probe before the adapter requests;
        // probe and continuation requests are served out of band and are not part
        // of the queued adapter exchange recorded for assertions.  The loop ends
        // once the queued adapter responses are exhausted and no new connection
        // arrives, so the test can join the server thread deterministically.
        let mut responses = responses.into_iter();
        let mut idle_rounds = 0_u32;
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if responses.len() == 0 {
                        idle_rounds += 1;
                        if idle_rounds >= 100 {
                            break;
                        }
                    } else {
                        idle_rounds = 0;
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("adapter listener accept failed: {error}"),
            };
            let request = read_http_request(&mut stream);
            let request_body = request
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            if request_body.contains("singularity_capability_probe") {
                let body = adapter_capability_probe_response(request_body);
                write_adapter_http_response(&mut stream, &body);
                continue;
            }
            let Some(body) = responses.next() else {
                break;
            };
            assert!(
                request
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains(endpoint)),
                "unexpected adapter endpoint: {request}"
            );
            captured.lock().expect("adapter request lock").push(request);
            write_adapter_http_response(&mut stream, &body);
        }
    });
    (format!("http://{address}"), requests, handle)
}

fn write_adapter_http_response(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .expect("write adapter response");
}

/// 用最小 probe 完成响应应答 capability probe 请求（含多轮 continuation）。
fn adapter_capability_probe_response(request_body: String) -> String {
    let request: serde_json::Value = serde_json::from_str(&request_body).expect("probe request");
    // Chat probe requests carry nested function schemas; Responses requests use
    // flat tool entries.  Detect the wire shape and reply in the same protocol.
    let chat_wire = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
    let responses_wire = request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call_output")
        });
    let continuation = chat_wire || responses_wire;
    let call_id = if continuation {
        "probe_call_continuation"
    } else {
        "probe_call_a"
    };
    if chat_wire || request.get("messages").is_some() {
        serde_json::json!({
            "id": if continuation { "capability_probe_continuation_response" } else { "capability_probe_response" },
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {"name": "singularity_capability_probe_a", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
        })
        .to_string()
    } else {
        let calls = vec![serde_json::json!({
            "type": "function_call",
            "call_id": call_id,
            "name": "singularity_capability_probe_a",
            "arguments": "{}",
        })];
        serde_json::json!({
            "id": if continuation { "capability_probe_continuation_response" } else { "capability_probe_response" },
            "object": "response",
            "status": "completed",
            "output": calls,
            "usage": {
                "input_tokens": 2,
                "output_tokens": 1,
                "total_tokens": 3,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0}
            }
        })
        .to_string()
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4_096];
    loop {
        let read = stream.read(&mut chunk).expect("read adapter request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let header = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line
                    .split_once(':')
                    .map(|(name, value)| (name.trim(), value.trim()))
                    .unwrap_or((line.trim(), ""));
                name.eq_ignore_ascii_case("Content-Length")
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        if bytes.len() >= header_end.saturating_add(content_length) {
            break;
        }
    }
    String::from_utf8(bytes).expect("adapter request utf8")
}

fn adapter_loop_with(
    base_url: String,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
) -> AgentLoop<AdapterFixtureProvider> {
    let provider = OpenAiProvider::new(adapter_test_config(base_url)).expect("adapter provider");
    AgentLoop::new(
        AdapterFixtureProvider {
            provider,
            seen_requests,
        },
        tool_broker(),
        read_policy(),
    )
    .with_workspace_tools(
        WorkspaceTools::new(env!("CARGO_MANIFEST_DIR")).expect("fixture workspace tools"),
    )
}

fn assistant_response(text: &str) -> ModelTurnResponse {
    ModelTurnResponse::completed("fixture_request", "fixture_response", text)
}

fn read_tool_response(path: &str) -> ModelTurnResponse {
    let mut response = assistant_response("reading");
    let arguments = json!({
        "path": path,
        "max_chars": null,
        "line_start": null,
        "line_end": null,
    });
    let call = ModelToolCall {
        tool_call_id: "read_call".to_string(),
        tool_name: singularity_tools::READ_TOOL.to_string(),
        arguments: arguments.clone(),
        raw_arguments: arguments.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    };
    response.tool_calls.push(call.clone());
    response
        .assistant_message
        .as_mut()
        .expect("assistant message")
        .tool_calls
        .push(call);
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
fn mismatched_assistant_tool_calls_fail_before_any_tool_execution() {
    let mut malformed = read_tool_response("README.md");
    malformed
        .assistant_message
        .as_mut()
        .expect("assistant message")
        .tool_calls
        .clear();
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = loop_with(
        vec![malformed],
        Arc::clone(&seen_requests),
        read_policy(),
        CancellationToken::new(),
        false,
    );

    let result = loop_.run(
        &AgentLoopInput::new("thread_mismatch", "turn_mismatch", "inspect"),
        AgentLoopCallbacks::none(),
    );

    assert_eq!(result.status, AgentStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("assistant_tool_calls_mismatch"))
    );
    assert_eq!(result.error_category, Some(ModelErrorCategory::JsonSchema));
    assert_eq!(
        result
            .provider_diagnostic
            .as_ref()
            .and_then(|diagnostic| diagnostic.code.as_deref()),
        Some("provider_response_invalid")
    );
    assert!(result.tool_results.is_empty());
    assert_eq!(seen_requests.lock().expect("requests").len(), 1);
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
fn chat_adapter_argument_parse_failure_reaches_typed_repair_without_execution() {
    let first = json!({
        "id": "chat_invalid_arguments",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_chat_invalid",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string();
    let second = json!({
        "id": "chat_repaired",
        "choices": [{
            "message": {"role": "assistant", "content": "recovered"},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let (base_url, network_requests, server) =
        adapter_response_server("/v1/chat/completions", vec![first, second]);
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = adapter_loop_with(
        format!("{base_url}/v1/chat/completions"),
        Arc::clone(&seen_requests),
    );
    let result = loop_.run(
        &AgentLoopInput::new("thread_chat_repair", "turn_chat_repair", "inspect"),
        AgentLoopCallbacks::none(),
    );
    server.join().expect("chat adapter server");

    assert_eq!(result.status, AgentStatus::Completed, "{:?}", result.error);
    assert_eq!(result.final_answer.as_deref(), Some("recovered"));
    assert_eq!(network_requests.lock().expect("network requests").len(), 2);
    assert_eq!(seen_requests.lock().expect("agent requests").len(), 2);
    let failure = result
        .tool_results
        .first()
        .expect("typed invalid arguments");
    assert_eq!(
        failure.error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    assert_eq!(
        failure
            .audit_metadata()
            .and_then(|audit| audit.get("executor_started"))
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    let requests = seen_requests.lock().expect("agent requests");
    let second_request = &requests[1];
    let assistant_call = second_request
        .messages
        .iter()
        .find(|message| message.role == singularity_model::ModelRole::Assistant)
        .and_then(|message| message.tool_calls.first())
        .expect("canonical rejected assistant call");
    assert_eq!(assistant_call.tool_call_id, "call_chat_invalid");
    assert_eq!(assistant_call.tool_name, "read");
    assert_eq!(assistant_call.arguments, json!({}));
    assert_eq!(assistant_call.raw_arguments, "{}");
    assert!(second_request.messages.iter().any(|message| {
        message.role == singularity_model::ModelRole::Tool
            && message.content.contains("invalid_tool_arguments")
    }));
}

#[test]
fn responses_adapter_schema_failure_reaches_typed_repair_without_execution() {
    let first = json!({
        "id": "responses_schema_mismatch",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": "call_responses_invalid",
            "name": "read",
            "arguments": {}
        }]
    })
    .to_string();
    let second = json!({
        "id": "responses_repaired",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "recovered"}]
        }]
    })
    .to_string();
    let (base_url, network_requests, server) =
        adapter_response_server("/v1/responses", vec![first, second]);
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let loop_ = adapter_loop_with(
        format!("{base_url}/v1/responses"),
        Arc::clone(&seen_requests),
    );
    let result = loop_.run(
        &AgentLoopInput::new(
            "thread_responses_repair",
            "turn_responses_repair",
            "inspect",
        ),
        AgentLoopCallbacks::none(),
    );
    server.join().expect("Responses adapter server");

    assert_eq!(result.status, AgentStatus::Completed, "{:?}", result.error);
    assert_eq!(result.final_answer.as_deref(), Some("recovered"));
    assert_eq!(network_requests.lock().expect("network requests").len(), 2);
    assert_eq!(seen_requests.lock().expect("agent requests").len(), 2);
    let failure = result
        .tool_results
        .first()
        .expect("typed invalid arguments");
    assert_eq!(
        failure.error_code.as_deref(),
        Some("invalid_tool_arguments")
    );
    assert_eq!(
        failure
            .audit_metadata()
            .and_then(|audit| audit.get("executor_started"))
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    let requests = seen_requests.lock().expect("agent requests");
    let second_request = &requests[1];
    let assistant_call = second_request
        .messages
        .iter()
        .find(|message| message.role == singularity_model::ModelRole::Assistant)
        .and_then(|message| message.tool_calls.first())
        .expect("canonical rejected assistant call");
    assert_eq!(assistant_call.tool_call_id, "call_responses_invalid");
    assert_eq!(assistant_call.tool_name, "read");
    assert_eq!(assistant_call.arguments, json!({}));
    assert_eq!(assistant_call.raw_arguments, "{}");
    assert!(second_request.messages.iter().any(|message| {
        message.role == singularity_model::ModelRole::Tool
            && message.content.contains("invalid_tool_arguments")
    }));
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
fn v7_and_v8_checkpoints_reject_pending_completed_fingerprint_overlap() {
    let ordinary_seen = Arc::new(Mutex::new(Vec::new()));
    let ordinary_loop = loop_with(
        vec![
            read_tool_response("missing-file-for-checkpoint-test"),
            assistant_response("done"),
        ],
        Arc::clone(&ordinary_seen),
        read_policy(),
        CancellationToken::new(),
        false,
    );
    let mut ready_checkpoint = None;
    let mut on_checkpoint = |event: TurnCheckpointEvent| -> Result<(), AgentLoopEventSinkError> {
        if matches!(event.phase, TurnCheckpointPhase::ToolCallsReady { .. }) {
            ready_checkpoint = Some(event.checkpoint);
        }
        Ok(())
    };
    let result = ordinary_loop.run(
        &AgentLoopInput::new("thread_overlap", "turn_overlap", "inspect"),
        AgentLoopCallbacks {
            on_event: None,
            on_checkpoint: Some(&mut on_checkpoint),
        },
    );
    assert_eq!(result.status, AgentStatus::Completed);
    let mut ordinary_payload = ready_checkpoint
        .expect("ordinary tool-call checkpoint")
        .encode()
        .expect("encode ordinary checkpoint");
    let ordinary_fingerprint = ordinary_payload["seen_tool_call_fingerprints"][0].clone();
    ordinary_payload["completed_tool_call_fingerprints"] = json!([ordinary_fingerprint]);
    let ordinary_error = TurnCheckpoint::decode(&ordinary_payload)
        .expect_err("ordinary checkpoint overlap must fail closed");
    assert!(ordinary_error.contains("overlaps completed fingerprint state"));

    let approval_seen = Arc::new(Mutex::new(Vec::new()));
    let approval_loop = loop_with(
        vec![read_tool_response("README.md")],
        Arc::clone(&approval_seen),
        approval_policy(),
        CancellationToken::new(),
        false,
    );
    let approval_result = approval_loop.run(
        &AgentLoopInput::new(
            "thread_overlap_approval",
            "turn_overlap_approval",
            "inspect",
        ),
        AgentLoopCallbacks::none(),
    );
    assert_eq!(approval_result.status, AgentStatus::Blocked);
    let pending = approval_result
        .pending_approvals
        .first()
        .expect("approval occurrence");
    let mut approval_payload = pending
        .encode_checkpoint()
        .expect("encode approval checkpoint");
    let approval_fingerprint = approval_payload["seen_tool_call_fingerprints"][0].clone();
    approval_payload["completed_tool_call_fingerprints"] = json!([approval_fingerprint]);
    let approval_error = PendingApprovalOccurrence::from_checkpoint_payload(
        pending.request().clone(),
        &approval_payload,
    )
    .expect_err("approval checkpoint overlap must fail closed");
    assert!(approval_error.contains("overlaps completed fingerprint state"));
    assert_eq!(ordinary_seen.lock().expect("ordinary requests").len(), 2);
    assert_eq!(approval_seen.lock().expect("approval requests").len(), 1);
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
