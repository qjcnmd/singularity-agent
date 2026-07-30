//! Provider adapter、Direct capability probe、协议投影和错误归因测试。

use schemars::schema_for;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelBlockerKind, ModelError,
    ModelErrorCategory, ModelErrorKind, ModelMessage, ModelPreferences, ModelProviderConfig,
    ModelRole, ModelToolCall, ModelToolParseStatus, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, OpenAiProvider, OpenAiProviderConfig, Provider,
    ProviderApiProtocol, ProviderAttemptEvent, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStatus, ProviderCapabilityCacheLookupResult,
    ProviderCapabilityMetadata, ProviderCapabilityProfile, ProviderConfigSnapshot,
    ProviderConfigSource, ProviderConfigurationStatus, ProviderErrorStage,
    ProviderProtocolContract, ProviderStreamEvent, ProviderStreamingCapability,
    ProviderToolReasoningMode, ToolChoiceMode, ToolChoicePolicy, chat_completions_endpoint,
    classify_model_error, resolve_provider_config, responses_endpoint, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

static CURRENT_DIR_LOCK: Mutex<()> = Mutex::new(());

struct CurrentDirRestore(PathBuf);

impl Drop for CurrentDirRestore {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore current dir");
    }
}

fn in_current_dir<T>(path: &std::path::Path, action: impl FnOnce() -> T) -> T {
    let _lock = CURRENT_DIR_LOCK.lock().expect("current dir lock");
    let original_dir = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(path).expect("enter synthetic project");
    let _restore = CurrentDirRestore(original_dir);
    action()
}

fn tool_call(id: &str, name: &str) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        arguments: serde_json::json!({"path": "README.md"}),
        raw_arguments: r#"{"path":"README.md"}"#.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}

fn provider_test_config(base_url: String) -> OpenAiProviderConfig {
    provider_config_with_base_url(chat_completions_endpoint(&base_url))
}

fn provider_auto_test_config(base_url: String) -> OpenAiProviderConfig {
    provider_config_with_base_url(base_url)
}

fn provider_config_with_base_url(base_url: String) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url,
        api_key: "sk-secret-value".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

fn capability_test_request(
    model_name: Option<&str>,
    strict: bool,
    max_tool_calls: u32,
) -> ModelTurnRequest {
    let mut request = ModelTurnRequest::new(
        "request_capability_test",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    if let Some(model_name) = model_name {
        request.model_preferences.model_name = Some(model_name.to_string());
    }
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    request.tool_choice.max_tool_calls = max_tool_calls;
    request.tool_choice.strict_tool_schema = strict;
    request
}

fn history_only_finalization_request(request_id: &str) -> ModelTurnRequest {
    let mut request = ModelTurnRequest::new(
        request_id,
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request
        .messages
        .push(ModelMessage::assistant_tool_calls(vec![tool_call(
            "call_read",
            "read",
        )]));
    let mut tool_result = ModelMessage::text(ModelRole::Tool, r#"{"ok":true}"#);
    tool_result.tool_call_id = Some("call_read".to_string());
    request.messages.push(tool_result);
    request.tool_choice = ToolChoicePolicy {
        mode: ToolChoiceMode::None,
        max_tool_calls: 0,
        strict_tool_schema: false,
    };
    request
}

fn single_response_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    let addr = listener.local_addr().expect("test provider address");
    thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().expect("accept test provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            if let Some(probe_body) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &probe_body, false);
                continue;
            }
            write_provider_response(&mut stream, status_line, body, false);
            break;
        }
    });
    format!("http://{addr}")
}

fn captured_request_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    let addr = listener.local_addr().expect("test provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().expect("accept test provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            if let Some(probe_body) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &probe_body, false);
                continue;
            }
            tx.send(request_body).expect("send request body");
            write_provider_response(&mut stream, status_line, body, false);
            break;
        }
    });
    (format!("http://{addr}"), rx)
}

fn responses_provider_server(
    actual_body: serde_json::Value,
) -> (String, Receiver<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Responses provider");
    let addr = listener.local_addr().expect("Responses provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept Responses request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/responses"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            requests.push((first_line, request_body.clone()));
            if let Some(body) = responses_capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, false);
                continue;
            }
            let body = actual_body.to_string();
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, false);
            tx.send(requests).expect("send Responses requests");
            break;
        }
    });
    (format!("http://{addr}"), rx)
}

fn responses_stream_server(
    chunks: Vec<Vec<u8>>,
    declared_length: Option<usize>,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Responses stream provider");
    let addr = listener
        .local_addr()
        .expect("Responses stream provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Responses stream request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/responses"));
        assert!(headers.contains("authorization: Bearer sk-secret-value"));
        tx.send(request_body)
            .expect("send Responses stream request");
        let length = declared_length
            .map(|length| format!("content-length: {length}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{length}\r\n"
        )
        .expect("write Responses stream headers");
        stream.flush().expect("flush Responses stream headers");
        for chunk in chunks {
            stream
                .write_all(&chunk)
                .expect("write Responses stream chunk");
            stream.flush().expect("flush Responses stream chunk");
            thread::sleep(Duration::from_millis(1));
        }
    });
    (format!("http://{addr}/v1/responses"), rx)
}

fn finalization_protocol_server() -> (String, Receiver<Vec<(String, String)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind finalization provider");
    let addr = listener
        .local_addr()
        .expect("finalization provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept finalization request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            requests.push((first_line.clone(), request_body.clone()));
            if first_line.contains("/v1/responses")
                && let Some(body) = responses_capability_probe_response(&request_body)
            {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, false);
                continue;
            }
            let body = if first_line.contains("/v1/responses") {
                serde_json::json!({
                    "id": "response_final",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "done"}]
                    }],
                    "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
                })
            } else {
                serde_json::json!({
                    "id": "chat_final",
                    "choices": [{
                        "message": {"role": "assistant", "content": "done"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
                })
            };
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body.to_string(), false);
            tx.send(requests).expect("send finalization requests");
            break;
        }
    });
    (format!("http://{addr}"), rx)
}

fn responses_to_chat_fallback_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind protocol fallback provider");
    let addr = listener
        .local_addr()
        .expect("protocol fallback provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut paths = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept protocol request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let (first_line, _, request_body) = read_provider_request(&mut reader);
            paths.push(first_line.clone());
            if first_line.contains("/v1/responses") {
                write_provider_response(
                    &mut stream,
                    "HTTP/1.1 404 Not Found",
                    r#"{"error":"unsupported endpoint"}"#,
                    false,
                );
                continue;
            }
            assert!(first_line.contains("/v1/chat/completions"));
            if let Some(body) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, false);
                continue;
            }
            write_provider_response(
                &mut stream,
                "HTTP/1.1 200 OK",
                &serde_json::json!({
                    "id": "chat_actual",
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call_read",
                                "type": "function",
                                "function": {"name": "read", "arguments": "{}"}
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                })
                .to_string(),
                false,
            );
            tx.send(paths).expect("send protocol fallback paths");
            break;
        }
    });
    (format!("http://{addr}"), rx)
}

fn protocol_status_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind protocol status provider");
    let addr = listener
        .local_addr()
        .expect("protocol status provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept protocol request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone protocol stream"));
        let (first_line, headers, _) = read_provider_request(&mut reader);
        assert!(headers.contains("authorization: Bearer sk-secret-value"));
        write_provider_response(&mut stream, status_line, body, false);
        tx.send(first_line).expect("send protocol request path");
    });
    (format!("http://{addr}"), rx)
}

fn configurable_probe_server(
    probe_responses: Vec<(&'static str, &'static str)>,
    actual_body: &'static str,
    actual_count: usize,
) -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind capability provider");
    let addr = listener.local_addr().expect("capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut probe_index = 0;
        let mut seen_requests = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept capability request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone capability stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            if is_capability_probe_continuation_request(&request_body) {
                let body = capability_probe_response(&request_body)
                    .expect("capability continuation response");
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &body, true);
                continue;
            }
            if request_body.contains("singularity_capability_probe") {
                let (status_line, body) = probe_responses
                    .get(probe_index)
                    .copied()
                    .expect("capability probe response configured");
                probe_index += 1;
                write_provider_response(&mut stream, status_line, body, true);
                if actual_count == 0 && probe_index == probe_responses.len() {
                    tx.send(seen_requests).expect("send probe-only requests");
                    break;
                }
                continue;
            }
            seen_requests.push(request_body);
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", actual_body, true);
            if seen_requests.len() == actual_count {
                tx.send(seen_requests)
                    .expect("send captured actual request");
                break;
            }
        }
    });
    (format!("http://{addr}"), rx)
}

fn strict_probe_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind strict capability provider");
    let addr = listener
        .local_addr()
        .expect("strict capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept strict capability request");
        let mut reader =
            BufReader::new(stream.try_clone().expect("clone strict capability stream"));
        let (first_line, headers, request_body) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/chat/completions"));
        assert!(headers.contains("authorization: Bearer sk-secret-value"));

        let request: serde_json::Value =
            serde_json::from_str(&request_body).expect("strict capability request JSON");
        let parameters = request
            .pointer("/tools/0/function/parameters")
            .expect("strict probe parameters");
        let branch = parameters
            .pointer("/oneOf/0")
            .unwrap_or(&serde_json::Value::Null);
        let valid_schema = parameters["oneOf"].as_array().map(Vec::len) == Some(2)
            && branch["type"] == "object"
            && branch["required"] == serde_json::json!(["probe", "values"])
            && branch["additionalProperties"] == false
            && branch["properties"]["probe"]["const"] == "schema_sentinel_alpha"
            && branch["properties"]["values"]["type"] == "array";
        let valid_roles = request["messages"][0]["role"] == "developer"
            && request["messages"][1]["role"] == "user";
        if valid_schema && valid_roles {
            write_provider_response(
                &mut stream,
                "HTTP/1.1 200 OK",
                PROBE_STRICT_PARALLEL_RESPONSE,
                true,
            );
        } else {
            write_provider_response(&mut stream, "HTTP/1.1 400 Bad Request", "{}", true);
            tx.send(vec![request_body])
                .expect("send invalid strict capability request");
            return;
        }
        let (mut stream, _) = listener
            .accept()
            .expect("accept strict capability continuation");
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .expect("clone strict capability continuation stream"),
        );
        let (_, _, continuation_body) = read_provider_request(&mut reader);
        let response = capability_probe_response(&continuation_body)
            .expect("strict capability continuation response");
        write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
        tx.send(vec![request_body, continuation_body])
            .expect("send strict capability requests");
    });
    (format!("http://{addr}"), rx)
}

fn reasoning_stabilization_probe_server(
    disabled_status: &'static str,
    disabled_body: &'static str,
    actual_body: &'static str,
    actual_count: usize,
) -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind reasoning capability provider");
    let addr = listener
        .local_addr()
        .expect("reasoning capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        let mut actual_seen = 0;
        loop {
            let (mut stream, _) = listener
                .accept()
                .expect("accept reasoning capability request");
            let mut reader = BufReader::new(
                stream
                    .try_clone()
                    .expect("clone reasoning capability stream"),
            );
            let (_, _, request_body) = read_provider_request(&mut reader);
            let request: serde_json::Value =
                serde_json::from_str(&request_body).expect("reasoning capability request JSON");
            requests.push(request_body);
            if is_reasoning_probe_continuation_request(
                requests.last().expect("continuation request"),
            ) {
                assert_eq!(request["thinking"]["type"], "disabled");
                let response =
                    capability_probe_response(requests.last().expect("continuation request body"))
                        .expect("reasoning capability continuation response");
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
                if actual_count == 0 {
                    tx.send(requests).expect("send reasoning probe requests");
                    break;
                }
                continue;
            }
            if request["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("singularity_capability_probe"))
                })
            }) {
                if request
                    .pointer("/thinking/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("disabled")
                {
                    write_provider_response(&mut stream, disabled_status, disabled_body, true);
                } else {
                    write_provider_response(
                        &mut stream,
                        "HTTP/1.1 200 OK",
                        PROBE_STRICT_PARALLEL_REASONING_RESPONSE,
                        true,
                    );
                }
                if actual_count == 0 && requests.len() == 2 {
                    tx.send(requests).expect("send reasoning probe requests");
                    break;
                }
                continue;
            }
            actual_seen += 1;
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", actual_body, true);
            if actual_seen == actual_count {
                tx.send(requests)
                    .expect("send reasoning negotiation requests");
                break;
            }
        }
    });
    (format!("http://{addr}"), rx)
}

fn strict_constraint_mismatch_probe_server(bad_arguments: &'static str) -> (String, Receiver<()>) {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("bind strict constraint capability provider");
    let addr = listener
        .local_addr()
        .expect("strict constraint capability provider address");
    let (tx, rx) = mpsc::channel();
    let bad_response = serde_json::json!({
        "id": "probe_strict_constraint_mismatch",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "probe_call_a",
                        "type": "function",
                        "function": {
                            "name": "singularity_capability_probe_a",
                            "arguments": bad_arguments
                        }
                    },
                    {
                        "id": "probe_call_b",
                        "type": "function",
                        "function": {
                            "name": "singularity_capability_probe_b",
                            "arguments": bad_arguments
                        }
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }]
    })
    .to_string();
    thread::spawn(move || {
        for (status_line, body) in [
            ("HTTP/1.1 200 OK", bad_response.as_str()),
            ("HTTP/1.1 200 OK", PROBE_BAD_ARGUMENTS_RESPONSE),
            ("HTTP/1.1 200 OK", bad_response.as_str()),
        ] {
            let (mut stream, _) = listener
                .accept()
                .expect("accept strict constraint capability request");
            let mut reader = BufReader::new(
                stream
                    .try_clone()
                    .expect("clone strict constraint capability stream"),
            );
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            assert!(request_body.contains("singularity_capability_probe"));
            write_provider_response(&mut stream, status_line, body, true);
        }
        let (mut stream, _) = listener
            .accept()
            .expect("accept strict constraint continuation request");
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .expect("clone strict constraint continuation stream"),
        );
        let (_, _, request_body) = read_provider_request(&mut reader);
        let response = capability_probe_response(&request_body)
            .expect("strict constraint continuation response");
        write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
        tx.send(())
            .expect("send strict constraint probe completion");
    });
    (format!("http://{addr}"), rx)
}

fn delayed_probe_server(
    probe_responses: Vec<(&'static str, &'static str)>,
    response_delay: Duration,
) -> (String, Receiver<Vec<String>>, Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind delayed capability provider");
    let addr = listener
        .local_addr()
        .expect("delayed capability provider address");
    let (request_tx, request_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut seen_requests = Vec::new();
        let mut expect_continuation = false;
        for (status_line, body) in probe_responses {
            let (mut stream, _) = listener
                .accept()
                .expect("accept delayed capability request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone capability stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            assert!(request_body.contains("singularity_capability_probe"));
            started_tx
                .send(())
                .expect("send capability request started");
            thread::sleep(response_delay);
            seen_requests.push(request_body);
            write_provider_response_best_effort(&mut stream, status_line, body, true);
            expect_continuation = status_line.contains("200 OK") && body.contains("\"tool_calls\"");
        }
        if expect_continuation {
            let (mut stream, _) = listener
                .accept()
                .expect("accept delayed capability continuation");
            let mut reader = BufReader::new(
                stream
                    .try_clone()
                    .expect("clone delayed capability continuation stream"),
            );
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            assert!(is_capability_probe_continuation_request(&request_body));
            started_tx
                .send(())
                .expect("send capability continuation started");
            thread::sleep(response_delay);
            let body = capability_probe_response(&request_body)
                .expect("delayed capability continuation response");
            seen_requests.push(request_body);
            write_provider_response_best_effort(&mut stream, "HTTP/1.1 200 OK", &body, true);
        }
        request_tx
            .send(seen_requests)
            .expect("send delayed capability requests");
    });
    (format!("http://{addr}"), request_rx, started_rx)
}

fn direct_only_probe_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind direct capability provider");
    let addr = listener
        .local_addr()
        .expect("direct capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("accept direct capability request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone direct stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            assert!(request_body.contains("singularity_capability_probe"));
            requests.push(request_body);
            write_provider_response(&mut stream, "HTTP/1.1 400 Bad Request", "{}", true);
        }
        tx.send(requests).expect("send direct capability requests");
    });
    (format!("http://{addr}"), rx)
}

fn persistent_probe_server(expected_requests: usize) -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind persistent capability provider");
    let addr = listener
        .local_addr()
        .expect("persistent capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..expected_requests {
            let (mut stream, _) = listener
                .accept()
                .expect("accept persistent capability request");
            let mut reader = BufReader::new(
                stream
                    .try_clone()
                    .expect("clone persistent capability stream"),
            );
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            let response = capability_probe_response(&request_body)
                .expect("persistent capability probe request");
            requests.push(request_body);
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
        }
        tx.send(requests)
            .expect("send persistent capability requests");
    });
    (format!("http://{addr}"), rx)
}

fn parallel_persistent_probe_server(
    expected_requests: usize,
) -> (String, Receiver<(usize, Vec<String>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind parallel capability provider");
    let addr = listener
        .local_addr()
        .expect("parallel capability provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for _ in 0..expected_requests {
            let (mut stream, _) = listener
                .accept()
                .expect("accept parallel capability request");
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let requests = Arc::clone(&requests);
            workers.push(thread::spawn(move || {
                let mut reader = BufReader::new(
                    stream
                        .try_clone()
                        .expect("clone parallel capability stream"),
                );
                let (first_line, headers, request_body) = read_provider_request(&mut reader);
                assert!(first_line.contains("/v1/chat/completions"));
                assert!(headers.contains("authorization: Bearer sk-secret-value"));
                requests
                    .lock()
                    .expect("parallel request list")
                    .push(request_body.clone());
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(100));
                let response = capability_probe_response(&request_body)
                    .expect("parallel capability probe request");
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.join().expect("join parallel capability request");
        }
        let requests = Arc::try_unwrap(requests)
            .expect("parallel request list ownership")
            .into_inner()
            .expect("parallel request list mutex");
        tx.send((maximum.load(Ordering::SeqCst), requests))
            .expect("send parallel capability requests");
    });
    (format!("http://{addr}"), rx)
}

fn cached_capability_rejection_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind invalidation provider");
    let addr = listener
        .local_addr()
        .expect("invalidation provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().expect("accept invalidation request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone invalidation stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            if let Some(response) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
            } else {
                write_provider_response(
                    &mut stream,
                    "HTTP/1.1 200 OK",
                    ACTUAL_TOOL_REASONING_RESPONSE,
                    true,
                );
            }
            requests.push(request_body);
        }
        tx.send(requests).expect("send invalidation requests");
    });
    (format!("http://{addr}"), rx)
}

fn ordinary_http_400_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ordinary 400 provider");
    let addr = listener
        .local_addr()
        .expect("ordinary 400 provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept ordinary 400 request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone ordinary 400 stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            if let Some(response) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &response, true);
            } else {
                write_provider_response(&mut stream, "HTTP/1.1 400 Bad Request", "{}", true);
            }
            requests.push(request_body);
        }
        tx.send(requests).expect("send ordinary 400 requests");
    });
    (format!("http://{addr}"), rx)
}

fn multi_model_probe_server(actual_count: usize) -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind multi-model provider");
    let addr = listener.local_addr().expect("multi-model provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut actual_requests = Vec::new();
        let mut all_requests = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept multi-model request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone multi-model stream"));
            let (first_line, headers, request_body) = read_provider_request(&mut reader);
            assert!(first_line.contains("/v1/chat/completions"));
            assert!(headers.contains("authorization: Bearer sk-secret-value"));
            all_requests.push(request_body.clone());
            if let Some(probe_body) = capability_probe_response(&request_body) {
                write_provider_response(&mut stream, "HTTP/1.1 200 OK", &probe_body, true);
                continue;
            }
            actual_requests.push(request_body);
            write_provider_response(&mut stream, "HTTP/1.1 200 OK", ACTUAL_DONE_RESPONSE, true);
            if actual_requests.len() == actual_count {
                tx.send(all_requests).expect("send multi-model requests");
                break;
            }
        }
    });
    (format!("http://{addr}"), rx)
}

fn multi_turn_probe_recovery_server() -> (String, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind multi-turn probe provider");
    let addr = listener
        .local_addr()
        .expect("multi-turn probe provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..7 {
            let (mut stream, _) = listener.accept().expect("accept multi-turn probe request");
            let mut reader =
                BufReader::new(stream.try_clone().expect("clone multi-turn probe stream"));
            let (_, _, request_body) = read_provider_request(&mut reader);
            requests.push(request_body.clone());
            let (status, body) = match index {
                0..=2 => ("HTTP/1.1 400 Bad Request", "{}".to_string()),
                3 => ("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE.to_string()),
                4 => ("HTTP/1.1 200 OK", PROBE_TEXT_RESPONSE.to_string()),
                5 => (
                    "HTTP/1.1 200 OK",
                    PROBE_STRICT_PARALLEL_RESPONSE.to_string(),
                ),
                6 => (
                    "HTTP/1.1 200 OK",
                    capability_probe_response(&request_body)
                        .expect("successful second multi-turn probe continuation"),
                ),
                _ => unreachable!(),
            };
            write_provider_response(&mut stream, status, &body, true);
        }
        tx.send(requests).expect("send multi-turn probe requests");
    });
    (format!("http://{addr}"), rx)
}

const PROBE_STRICT_PARALLEL_RESPONSE: &str = r#"{
    "id": "probe_strict_parallel",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "probe_call_a",
                    "type": "function",
                    "function": {
                        "name": "singularity_capability_probe_a",
                        "arguments": "{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}"
                    }
                },
                {
                    "id": "probe_call_b",
                    "type": "function",
                    "function": {
                        "name": "singularity_capability_probe_b",
                        "arguments": "{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}"
                    }
                }
            ]
        },
        "finish_reason": "tool_calls"
    }],
    "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
}"#;

const PROBE_STRICT_PARALLEL_REASONING_RESPONSE: &str = r#"{
    "id": "probe_strict_parallel_reasoning",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "reasoning_content": "fixed private probe reasoning",
            "tool_calls": [
                {
                    "id": "probe_call_a",
                    "type": "function",
                    "function": {
                        "name": "singularity_capability_probe_a",
                        "arguments": "{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}"
                    }
                },
                {
                    "id": "probe_call_b",
                    "type": "function",
                    "function": {
                        "name": "singularity_capability_probe_b",
                        "arguments": "{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}"
                    }
                }
            ]
        },
        "finish_reason": "tool_calls"
    }],
    "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
}"#;

const PROBE_STRICT_SINGLE_RESPONSE: &str = r#"{
    "id": "probe_strict_single",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "probe_call_a",
                "type": "function",
                "function": {
                    "name": "singularity_capability_probe_a",
                    "arguments": "{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}"
                }
            }
            ]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

const PROBE_NON_STRICT_SINGLE_RESPONSE: &str = r#"{
    "id": "probe_non_strict_single",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "probe_call_a",
                "type": "function",
                "function": {
                    "name": "singularity_capability_probe_a",
                    "arguments": "{}"
                }
            }]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

const PROBE_TEXT_RESPONSE: &str = r#"{
    "id": "probe_text",
    "choices": [{
        "message": {"role": "assistant", "content": "call singularity_capability_probe_a"},
        "finish_reason": "stop"
    }]
}"#;

const PROBE_BAD_ARGUMENTS_RESPONSE: &str = r#"{
    "id": "probe_bad_arguments",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "probe_call_a",
                "type": "function",
                "function": {"name": "singularity_capability_probe_a", "arguments": "[]"}
            }]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

const ACTUAL_DONE_RESPONSE: &str = r#"{
    "id": "actual_response",
    "choices": [{
        "message": {"role": "assistant", "content": "done"},
        "finish_reason": "stop"
    }]
}"#;

const ACTUAL_TOOL_REASONING_RESPONSE: &str = r#"{
    "id": "actual_tool_reasoning_response",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "reasoning_content": "private reasoning that requires replay",
            "tool_calls": [{
                "id": "actual_tool_call",
                "type": "function",
                "function": {"name": "read", "arguments": "{}"}
            }]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

const CHAT_HISTORY_REASONING_RESPONSE: &str = r#"{
    "id": "chat_history_reasoning",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "done",
            "reasoning_content": "private reasoning that requires replay"
        },
        "finish_reason": "stop"
    }]
}"#;

fn sequence_response_server(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sequence provider");
    let addr = listener.local_addr().expect("sequence provider address");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for (attempt, (status_line, body)) in responses.into_iter().enumerate() {
            loop {
                let (mut stream, _) = listener.accept().expect("accept sequence provider request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let (_, _, request_body) = read_provider_request(&mut reader);
                if let Some(probe_body) = capability_probe_response(&request_body) {
                    write_provider_response(&mut stream, "HTTP/1.1 200 OK", &probe_body, true);
                    continue;
                }
                tx.send(attempt + 1).expect("send provider attempt");
                write_provider_response(&mut stream, status_line, body, true);
                break;
            }
        }
    });
    (format!("http://{addr}"), rx)
}

fn read_provider_request(reader: &mut BufReader<TcpStream>) -> (String, String, String) {
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .expect("read request line");
    let mut headers = String::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().unwrap_or(0);
        }
        headers.push_str(&line);
    }
    let mut request_body = vec![0; content_length];
    reader
        .read_exact(&mut request_body)
        .expect("read request body");
    (
        first_line,
        headers,
        String::from_utf8(request_body).expect("utf8 request body"),
    )
}

fn write_provider_response(stream: &mut TcpStream, status_line: &str, body: &str, close: bool) {
    let connection = if close { "connection: close\r\n" } else { "" };
    write!(
        stream,
        "{status_line}\r\n{connection}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("write provider response");
}

fn write_provider_response_best_effort(
    stream: &mut TcpStream,
    status_line: &str,
    body: &str,
    close: bool,
) {
    let connection = if close { "connection: close\r\n" } else { "" };
    let _ = write!(
        stream,
        "{status_line}\r\n{connection}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
}

fn capability_probe_response(request_body: &str) -> Option<String> {
    let request: serde_json::Value = serde_json::from_str(request_body).ok()?;
    let tools = request.get("tools")?.as_array()?;
    let names = tools
        .iter()
        .filter_map(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
        })
        .collect::<Vec<_>>();
    let continuation = request
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"));
    if !names.contains(&"singularity_capability_probe_a") {
        return None;
    }
    let strict = tools.iter().any(|tool| {
        tool.pointer("/function/strict")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    });
    if continuation {
        let arguments = if strict {
            serde_json::json!({"probe": "schema_sentinel_alpha", "values": [7, 7]})
        } else {
            serde_json::json!({})
        };
        return Some(capability_probe_continuation_response(
            "singularity_capability_probe_a",
            arguments,
        ));
    }
    if strict {
        return Some(
            if names.contains(&"singularity_capability_probe_b") {
                PROBE_STRICT_PARALLEL_RESPONSE
            } else {
                PROBE_STRICT_SINGLE_RESPONSE
            }
            .to_string(),
        );
    }
    let tool_calls = if names.contains(&"singularity_capability_probe_b") {
        vec![
            serde_json::json!({
                "id": "probe_call_a",
                "type": "function",
                "function": {"name": "singularity_capability_probe_a", "arguments": "{}"}
            }),
            serde_json::json!({
                "id": "probe_call_b",
                "type": "function",
                "function": {"name": "singularity_capability_probe_b", "arguments": "{}"}
            }),
        ]
    } else {
        vec![serde_json::json!({
            "id": "probe_call_a",
            "type": "function",
            "function": {"name": "singularity_capability_probe_a", "arguments": "{}"}
        })]
    };
    Some(
        serde_json::json!({
            "id": "capability_probe_response",
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": tool_calls},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
        })
        .to_string(),
    )
}

fn responses_capability_probe_response(request_body: &str) -> Option<String> {
    let request: serde_json::Value = serde_json::from_str(request_body).ok()?;
    let tools = request.get("tools")?.as_array()?;
    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    let continuation = request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call_output")
        });
    let (tool_name, arguments) = if names.contains(&"singularity_capability_probe_a") {
        let strict = tools
            .iter()
            .any(|tool| tool.get("strict").and_then(serde_json::Value::as_bool) == Some(true));
        (
            "singularity_capability_probe_a",
            if strict {
                serde_json::json!({"probe": "schema_sentinel_alpha", "values": [7, 7]})
            } else {
                serde_json::json!({})
            },
        )
    } else {
        return None;
    };
    let mut calls = vec![serde_json::json!({
        "type": "function_call",
        "call_id": if continuation { "probe_call_continuation" } else { "probe_call_a" },
        "name": tool_name,
        "arguments": arguments.to_string(),
    })];
    if !continuation
        && names.contains(&"singularity_capability_probe_b")
        && request["parallel_tool_calls"] == true
    {
        let strict = tools
            .iter()
            .any(|tool| tool.get("strict").and_then(serde_json::Value::as_bool) == Some(true));
        calls.push(serde_json::json!({
            "type": "function_call",
            "call_id": "probe_call_b",
            "name": "singularity_capability_probe_b",
            "arguments": if strict {
                serde_json::json!({"probe": "schema_sentinel_alpha", "values": [7, 7]}).to_string()
            } else {
                serde_json::json!({}).to_string()
            },
        }));
    }
    Some(
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
        .to_string(),
    )
}

fn is_capability_probe_continuation_request(request_body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(request_body)
        .ok()
        .and_then(|request| request.get("messages").cloned())
        .and_then(|messages| messages.as_array().cloned())
        .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"))
}

fn is_reasoning_probe_continuation_request(request_body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(request_body)
        .ok()
        .and_then(|request| request.get("messages").cloned())
        .and_then(|messages| messages.as_array().cloned())
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message["role"] == "tool"
                    && message["tool_call_id"]
                        .as_str()
                        .is_some_and(|tool_call_id| tool_call_id.starts_with("probe_"))
            })
        })
}

fn capability_probe_continuation_response(tool_name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "id": "capability_probe_continuation_response",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "probe_continuation_call",
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": arguments.to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
    })
    .to_string()
}

#[test]
fn model_turn_request_serializes_provider_boundary_fields() {
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let value = serde_json::to_value(&request).expect("serialize model request");

    assert_eq!(value["request_id"], "request_1");
    assert_eq!(value["messages"][0]["role"], "user");

    let response = ModelTurnResponse::completed("request_1", "response_1", "done");
    assert_eq!(response.status, ModelTurnStatus::Success);
}

#[test]
fn model_turn_schema_excludes_runtime_and_trace_metadata() {
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let value = serde_json::to_value(&request).expect("serialize model request");

    assert_eq!(value["tools"], serde_json::json!([]));
    assert_eq!(value["tool_choice"]["mode"], "auto");
    for excluded_field in [
        "run_id",
        "session_id",
        "task_id",
        "phase_id",
        "action_id",
        "context_metadata",
        "policy_metadata",
        "trace_metadata",
    ] {
        assert!(value.get(excluded_field).is_none());
    }

    let response = ModelTurnResponse::completed("request_1", "response_1", "done");
    let response_value = serde_json::to_value(&response).expect("serialize model response");

    assert_eq!(response_value["assistant_message"]["role"], "assistant");
    assert_eq!(response_value["tool_calls"], serde_json::json!([]));
    assert_eq!(response_value["usage"]["total_tokens"], 0);
    assert_eq!(response_value["request_id"], "request_1");
    assert_eq!(response_value["response_id"], "response_1");
    assert_eq!(response_value["status"], "success");
    for removed_field in ["latency_ms", "trace_event_ids", "metadata"] {
        assert!(
            !response_value
                .as_object()
                .unwrap()
                .contains_key(removed_field)
        );
    }
}

#[test]
fn provider_config_validation_reports_missing_boundary_fields() {
    let result = validate_provider_config(&ModelProviderConfig {
        provider_name: None,
        model_name: Some("gpt-test".to_string()),
        base_url_present: false,
        api_key_present: false,
    });

    assert!(!result.valid);
    assert_eq!(
        result.errors,
        vec![
            "api_key_required",
            "base_url_required",
            "provider_name_required"
        ]
    );
}

#[test]
fn provider_config_snapshot_is_atomic_immutable_and_secret_safe() {
    let mut reads = std::collections::HashMap::<String, usize>::new();
    let snapshot = ProviderConfigSnapshot::capture(|name| {
        let count = reads.entry(name.to_string()).or_default();
        *count += 1;
        assert_eq!(*count, 1, "provider setting {name} was read more than once");
        match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("snapshot-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("https://snapshot-provider.example/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("snapshot-secret".to_string()),
            _ => None,
        }
    });

    assert_eq!(
        snapshot.source(),
        Some(ProviderConfigSource::ProcessEnvironment)
    );
    assert_eq!(
        snapshot.redacted_config().model_name.as_deref(),
        Some("snapshot-model")
    );
    assert!(snapshot.configuration().configured);
    assert!(snapshot.provider().is_ok());
    assert!(snapshot.snapshot_id().starts_with("provider_snapshot_"));
    let debug = format!("{snapshot:?}");
    for secret in ["snapshot-secret", "snapshot-provider.example"] {
        assert!(!debug.contains(secret));
        assert!(!snapshot.snapshot_id().contains(secret));
    }

    let same_config = ProviderConfigSnapshot::capture(|name| match name {
        "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
        "SINGULARITY_MODEL" => Some("snapshot-model".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://snapshot-provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("snapshot-secret".to_string()),
        _ => None,
    });
    assert_ne!(snapshot.snapshot_id(), same_config.snapshot_id());
}

#[test]
fn provider_config_snapshot_preserves_the_original_configuration_error() {
    let temp = tempfile::tempdir().expect("temp dir");
    let snapshot = in_current_dir(temp.path(), || ProviderConfigSnapshot::capture(|_| None));

    assert_eq!(snapshot.source(), None);
    assert!(!snapshot.configuration().configured);
    let first = snapshot.provider().expect_err("missing provider config");
    let second = snapshot
        .provider()
        .expect_err("same missing provider config");
    assert_eq!(first, second);
    assert!(first.message.contains("SINGULARITY_MODEL"));
    assert_eq!(
        first.error.code.as_deref(),
        Some("provider_configuration_missing")
    );
    assert_eq!(
        first.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
}

#[test]
fn process_env_provider_values_fail_before_adapter_attempt_and_redact_input() {
    for (name, malformed) in [
        ("SINGULARITY_MODEL", "gpt-test\r"),
        ("SINGULARITY_BASE_URL", "https://provider.example/v1\r"),
        ("SINGULARITY_API_KEY", "sk-secret-value\r"),
        ("SINGULARITY_MODEL", "gpt\n-test"),
        ("SINGULARITY_BASE_URL", "https://provider.example/v1\0"),
    ] {
        let snapshot = ProviderConfigSnapshot::capture(|candidate| match candidate {
            "SINGULARITY_MODEL" => Some(if name == "SINGULARITY_MODEL" {
                malformed.to_string()
            } else {
                "gpt-test".to_string()
            }),
            "SINGULARITY_BASE_URL" => Some(if name == "SINGULARITY_BASE_URL" {
                malformed.to_string()
            } else {
                "https://provider.example/v1".to_string()
            }),
            "SINGULARITY_API_KEY" => Some(if name == "SINGULARITY_API_KEY" {
                malformed.to_string()
            } else {
                "sk-secret-value".to_string()
            }),
            _ => None,
        });

        assert!(!snapshot.configuration().configured);
        let error = snapshot
            .provider()
            .expect_err("malformed process environment must fail before provider creation");
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_configuration_invalid")
        );
        assert_eq!(
            error.error.stage,
            Some(ProviderErrorStage::ClientInitialization)
        );
        assert!(
            error.provider_attempt_metadata.is_none(),
            "configuration rejection must not create provider attempts"
        );
        assert!(!error.message.contains(malformed));
        assert!(
            !serde_json::to_string(&error.error)
                .expect("serialize configuration error")
                .contains(malformed)
        );
    }
}

#[test]
fn process_env_provider_values_reject_boundary_whitespace() {
    for (name, malformed) in [
        ("SINGULARITY_MODEL", " gpt-test"),
        ("SINGULARITY_BASE_URL", "https://provider.example/v1 "),
        ("SINGULARITY_API_KEY", "sk-secret-value\t"),
    ] {
        let error = OpenAiProviderConfig::from_env(|candidate| match candidate {
            "SINGULARITY_MODEL" => Some(if name == "SINGULARITY_MODEL" {
                malformed.to_string()
            } else {
                "gpt-test".to_string()
            }),
            "SINGULARITY_BASE_URL" => Some(if name == "SINGULARITY_BASE_URL" {
                malformed.to_string()
            } else {
                "https://provider.example/v1".to_string()
            }),
            "SINGULARITY_API_KEY" => Some(if name == "SINGULARITY_API_KEY" {
                malformed.to_string()
            } else {
                "sk-secret-value".to_string()
            }),
            _ => None,
        })
        .expect_err("boundary whitespace must be rejected");

        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_configuration_invalid")
        );
        assert!(!error.message.contains(malformed));
    }
}

#[test]
fn provider_response_decode_and_envelope_failures_have_stable_safe_diagnostics() {
    let malformed_url = single_response_server("HTTP/1.1 200 OK", "not-json");
    let malformed =
        OpenAiProvider::new(provider_test_config(malformed_url)).expect("malformed provider");
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let decode_error = malformed
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("decode failure");
    assert_eq!(
        decode_error.error.code.as_deref(),
        Some("provider_response_json_decode_failed")
    );
    assert_eq!(
        decode_error.error.stage,
        Some(ProviderErrorStage::ResponseJsonDecode)
    );
    let decode_metadata = decode_error
        .provider_attempt_metadata
        .as_ref()
        .expect("decode attempt metadata");
    assert_eq!(decode_metadata.attempt_count, 1);
    assert_eq!(decode_metadata.retry_count, 0);
    let [decode_occurrence] = decode_metadata.occurrences.as_slice() else {
        panic!("one decode failure occurrence expected");
    };
    assert_eq!(
        decode_occurrence.terminal_status,
        ProviderAttemptStatus::Error
    );
    assert_eq!(
        decode_occurrence.error_stage,
        Some(ProviderErrorStage::ResponseJsonDecode)
    );
    assert_eq!(
        decode_occurrence.diagnostic_code.as_deref(),
        Some("provider_response_json_decode_failed")
    );
    assert!(decode_occurrence.request_send_to_headers_ms.is_some());
    assert!(decode_occurrence.time_to_first_text_delta_ms.is_none());

    let missing_choices_url = single_response_server("HTTP/1.1 200 OK", r#"{"id":"response_1"}"#);
    let missing_choices = OpenAiProvider::new(provider_test_config(missing_choices_url))
        .expect("missing choices provider");
    let envelope_error = missing_choices
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("envelope failure");
    assert_eq!(
        envelope_error.error.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(
        envelope_error.error.stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert_eq!(
        envelope_error.error.validation_errors,
        vec!["response_choices_missing"]
    );
    let envelope_metadata = envelope_error
        .provider_attempt_metadata
        .as_ref()
        .expect("envelope attempt metadata");
    let [envelope_occurrence] = envelope_metadata.occurrences.as_slice() else {
        panic!("one response validation occurrence expected");
    };
    assert_eq!(
        envelope_occurrence.error_stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert_eq!(
        envelope_occurrence.diagnostic_code.as_deref(),
        Some("provider_response_invalid")
    );
    let serialized = serde_json::to_string(&envelope_error.error).expect("serialize error");
    assert!(!serialized.contains("hello"));
    assert!(!serialized.contains("not-json"));
}

#[test]
fn process_env_api_key_cannot_mix_with_project_env_model_and_base_url() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        "SINGULARITY_MODEL=project-model\nSINGULARITY_BASE_URL=https://project-provider.example/v1\n",
    )
    .expect("write synthetic project env");
    let result = in_current_dir(temp.path(), || {
        OpenAiProviderConfig::from_env(|name| match name {
            "SINGULARITY_API_KEY" => Some("process-secret".to_string()),
            _ => None,
        })
        .map_err(Box::new)
    });

    let error = result.expect_err("mixed provider sources must be rejected");
    assert_eq!(error.error.kind, ModelErrorKind::InvalidRequest);
    assert!(error.message.contains("SINGULARITY_MODEL"));
    assert!(!error.message.contains("project-model"));
    assert!(!error.message.contains("project-provider.example"));
    assert!(!error.message.contains("process-secret"));
}

#[test]
fn provider_status_does_not_fill_process_layer_from_project_env() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        "SINGULARITY_MODEL=project-model\nSINGULARITY_BASE_URL=https://project-provider.example/v1\n",
    )
    .expect("write synthetic project env");
    let resolution = in_current_dir(temp.path(), || {
        resolve_provider_config(|name| match name {
            "SINGULARITY_API_KEY" => Some("process-secret".to_string()),
            _ => None,
        })
    });

    assert_eq!(
        resolution.source,
        Some(ProviderConfigSource::ProcessEnvironment)
    );
    let config = resolution.config;
    assert_eq!(config.model_name, None);
    assert!(!config.base_url_present);
    assert!(config.api_key_present);
    assert_eq!(config.provider_name.as_deref(), Some("openai_compatible"));
}

#[test]
fn complete_project_env_configuration_has_project_file_provenance() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        concat!(
            "SINGULARITY_MODEL_PROVIDER=openai_compatible\n",
            "SINGULARITY_MODEL=project-model\n",
            "SINGULARITY_BASE_URL=https://project-provider.example/v1\n",
            "SINGULARITY_API_KEY=project-secret\n",
        ),
    )
    .expect("write synthetic project env");
    let result = in_current_dir(temp.path(), || {
        OpenAiProviderConfig::from_env(|_| None).map_err(Box::new)
    });

    let config = result.expect("project provider config");
    assert_eq!(config.source, ProviderConfigSource::ProjectEnvFile);
    assert_eq!(config.provider_name, "openai_compatible");
    assert_eq!(config.model_name, "project-model");
}

#[test]
fn project_env_crlf_line_endings_are_parsed_without_control_characters() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        concat!(
            "SINGULARITY_MODEL_PROVIDER=openai_compatible\r\n",
            "SINGULARITY_MODEL=project-model\r\n",
            "SINGULARITY_BASE_URL=https://project-provider.example/v1\r\n",
            "SINGULARITY_API_KEY=project-secret\r\n",
        ),
    )
    .expect("write CRLF project env");

    let config = in_current_dir(temp.path(), || {
        OpenAiProviderConfig::from_env(|_| None).expect("CRLF dotenv should parse")
    });

    assert_eq!(config.source, ProviderConfigSource::ProjectEnvFile);
    assert_eq!(config.model_name, "project-model");
    assert_eq!(config.base_url, "https://project-provider.example/v1");
}

#[test]
fn project_env_provider_values_reject_boundary_whitespace() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        concat!(
            "SINGULARITY_MODEL= project-model\r\n",
            "SINGULARITY_BASE_URL=https://project-provider.example/v1\r\n",
            "SINGULARITY_API_KEY=project-secret\r\n",
        ),
    )
    .expect("write invalid project env");

    let error = in_current_dir(temp.path(), || {
        OpenAiProviderConfig::from_env(|_| None).expect_err("dotenv boundary whitespace")
    });

    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
    assert!(error.message.contains("SINGULARITY_MODEL"));
    assert!(!error.message.contains("project-model"));
    assert!(!error.message.contains("project-provider.example"));
    assert!(!error.message.contains("project-secret"));
}

#[test]
fn project_env_fields_are_not_combined_across_env_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        temp.path().join(".env"),
        concat!(
            "SINGULARITY_MODEL=parent-model\n",
            "SINGULARITY_BASE_URL=https://parent-provider.example/v1\n",
        ),
    )
    .expect("write parent synthetic env");
    let child = temp.path().join("child");
    std::fs::create_dir(&child).expect("create child project");
    std::fs::write(child.join(".env"), "SINGULARITY_API_KEY=child-secret\n")
        .expect("write child synthetic env");

    let result = in_current_dir(&child, || {
        OpenAiProviderConfig::from_env(|_| None).map_err(Box::new)
    });

    let error = result.expect_err("project env files must not be combined");
    assert!(error.message.contains("SINGULARITY_MODEL"));
    assert!(error.message.contains("source=project_env"));
    assert!(!error.message.contains("parent-model"));
    assert!(!error.message.contains("parent-provider.example"));
    assert!(!error.message.contains("child-secret"));
}

#[test]
fn openai_provider_config_uses_redacted_status_and_endpoint_rules() {
    assert_eq!(
        chat_completions_endpoint("https://provider.example/v1"),
        "https://provider.example/v1/chat/completions"
    );
    assert_eq!(
        chat_completions_endpoint("https://provider.example/chat/completions"),
        "https://provider.example/chat/completions"
    );
    assert_eq!(
        chat_completions_endpoint("https://provider.example/api"),
        "https://provider.example/api/v1/chat/completions"
    );
    assert_eq!(
        responses_endpoint("https://provider.example/v1"),
        "https://provider.example/v1/responses"
    );
    assert_eq!(
        responses_endpoint("https://provider.example/v1/chat/completions"),
        "https://provider.example/v1/responses"
    );
    assert_eq!(
        responses_endpoint("https://provider.example/api"),
        "https://provider.example/api/v1/responses"
    );

    let config = OpenAiProviderConfig::from_env(|name| match name {
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        _ => None,
    })
    .expect("provider config");
    let status = config.redacted_status();
    let serialized = serde_json::to_string(&status).expect("serialize status");

    assert_eq!(config.provider_name, "openai_compatible");
    assert_eq!(config.source, ProviderConfigSource::ProcessEnvironment);
    assert!(status.configured);
    assert_eq!(status.api_key_status, "present(redacted)");
    assert!(!serialized.contains("sk-secret-value"));
    assert!(!serialized.contains("provider.example"));
}

#[test]
fn provider_config_rejects_an_unregistered_provider_instead_of_using_openai_transport() {
    let error = OpenAiProviderConfig::from_env(|name| match name {
        "SINGULARITY_MODEL_PROVIDER" => Some("unregistered_provider".to_string()),
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        _ => None,
    })
    .expect_err("unknown provider must fail closed");

    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_adapter_unsupported")
    );
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
    assert_eq!(
        error.error.provider_name.as_deref(),
        Some("unregistered_provider")
    );
    assert!(!error.message.contains("sk-secret-value"));
    assert!(!error.message.contains("provider.example"));
}

#[test]
fn openai_provider_negotiates_responses_api_and_replays_typed_function_items() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_actual",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": "call_read",
            "name": "read",
            "arguments": "{}"
        }],
        "usage": {
            "input_tokens": 9,
            "output_tokens": 4,
            "total_tokens": 13,
            "input_tokens_details": {"cached_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 0}
        }
    }));
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();
    let negotiation = provider
        .negotiate_tool_capabilities(&ModelPreferences::default(), &cancellation)
        .expect("Responses capability negotiation");

    assert_eq!(
        negotiation.metadata.api_protocol,
        ProviderApiProtocol::OpenAiResponses
    );
    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::StrictParallel
    );
    assert_eq!(
        negotiation.contract.tool_reasoning_mode,
        ProviderToolReasoningMode::DisabledForToolCalls
    );

    let request = capability_test_request(None, false, 2);
    let response = provider
        .complete(&request, &cancellation)
        .expect("Responses completion");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(response.tool_calls[0].tool_call_id, "call_read");
    assert_eq!(response.usage.input_tokens, 9);
    assert_eq!(response.usage.cached_input_tokens, 2);

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Responses requests");
    assert_eq!(captured.len(), 3);
    let first: serde_json::Value =
        serde_json::from_str(&captured[0].1).expect("first Responses probe JSON");
    assert_eq!(first["store"], false);
    assert_eq!(first["reasoning"]["effort"], "none");
    assert_eq!(first["tool_choice"], "auto");
    assert_eq!(first["parallel_tool_calls"], true);
    assert!(
        first["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.contains("capability probe"))
    );
    assert_eq!(first["input"][0]["role"], "user");
    assert!(
        first["input"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["role"] != "developer"))
    );
    assert_eq!(first["tools"][0]["type"], "function");
    assert_eq!(first["tools"][0]["strict"], true);
    assert!(first["tools"][0].get("function").is_none());

    let continuation: serde_json::Value =
        serde_json::from_str(&captured[1].1).expect("Responses continuation JSON");
    let continuation_items = continuation["input"]
        .as_array()
        .expect("Responses continuation items");
    assert!(
        continuation_items
            .iter()
            .any(|item| item["type"] == "function_call")
    );
    assert!(
        continuation_items
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );

    let actual: serde_json::Value =
        serde_json::from_str(&captured[2].1).expect("actual Responses request JSON");
    assert_eq!(actual["tools"][0]["name"], "read");
    assert_eq!(actual["tools"][0]["strict"], false);
    assert_eq!(actual["tool_choice"], "auto");
    assert_eq!(actual["reasoning"]["effort"], "none");
}

#[test]
fn openai_responses_stream_aggregates_deltas_and_requires_completed_envelope() {
    let delta_one = serde_json::json!({
        "type": "response.output_text.delta",
        "delta": "hel"
    });
    let delta_two = serde_json::json!({
        "type": "response.output_text.delta",
        "delta": "lo"
    });
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "response_stream_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }],
            "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
        }
    });
    let body = format!(
        "event: response.output_text.delta\r\ndata: {delta_one}\r\n\r\nevent: response.output_text.delta\r\ndata: {delta_two}\r\n\r\nevent: response.completed\r\ndata: {completed}\r\n\r\n"
    );
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let provider = OpenAiProvider::new(provider_config_with_base_url(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let cancellation = singularity_core::CancellationToken::new();
    let mut events = Vec::new();
    let response = provider
        .complete_stream(&request, &cancellation, &mut |event| events.push(event))
        .expect("Responses stream completion");

    assert_eq!(
        events,
        vec![
            ProviderStreamEvent::OutputTextDelta {
                delta: "hel".to_string()
            },
            ProviderStreamEvent::OutputTextDelta {
                delta: "lo".to_string()
            }
        ]
    );
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("hello")
    );
    let payload: serde_json::Value = serde_json::from_str(
        &requests
            .recv_timeout(Duration::from_secs(1))
            .expect("stream request"),
    )
    .expect("stream request JSON");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["store"], false);
    let metadata = response
        .provider_attempt_metadata
        .as_ref()
        .expect("stream attempt metadata");
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one HTTP attempt occurrence expected");
    };
    assert_eq!(
        occurrence.operation_phase,
        ProviderAttemptOperationPhase::Completion
    );
    assert_eq!(occurrence.provider_name, "openai_compatible");
    assert_eq!(occurrence.model_name, "gpt-test");
    assert_eq!(
        occurrence.actual_api_protocol,
        ProviderApiProtocol::OpenAiResponses
    );
    assert_eq!(occurrence.attempt_index, 1);
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Ok);
    assert!(occurrence.request_send_to_headers_ms.is_some());
    assert!(occurrence.queue_duration_ms.is_none());
    assert!(occurrence.time_to_first_text_delta_ms.is_some());
    assert!(!occurrence.retry_scheduled);
    assert!(occurrence.retry_backoff_ms.is_none());
    assert!(occurrence.error_category.is_none());
    assert!(occurrence.error_stage.is_none());
    assert!(occurrence.diagnostic_code.is_none());
    assert_eq!(
        occurrence.usage.as_ref().map(|usage| usage.total_tokens),
        Some(3)
    );
}

#[test]
fn openai_responses_stream_maps_terminal_failures_and_protocol_failures() {
    let cases = [
        (
            "error",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"provider_error\",\"message\":\"secret raw failure\"}}\n\n",
            "responses_stream_error",
        ),
        (
            "failed",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            "responses_stream_failed",
        ),
        (
            "incomplete",
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\"}}\n\n",
            "responses_stream_incomplete",
        ),
        (
            "missing_terminal",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "responses_stream_terminal_missing",
        ),
        (
            "malformed",
            "event: response.output_text.delta\ndata: {not-json}\n\n",
            "responses_stream_malformed",
        ),
    ];
    for (name, body, expected_code) in cases {
        let chunks = body
            .as_bytes()
            .chunks(2)
            .map(|chunk| chunk.to_vec())
            .collect();
        let (base_url, requests) = responses_stream_server(chunks, None);
        let provider =
            OpenAiProvider::new(provider_config_with_base_url(base_url)).expect("provider");
        let request = ModelTurnRequest::new(
            format!("request_stream_{name}"),
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        let error = provider
            .complete_stream(
                &request,
                &singularity_core::CancellationToken::new(),
                &mut |_| {},
            )
            .expect_err("stream must fail closed");
        assert_eq!(error.error.code.as_deref(), Some(expected_code), "{name}");
        assert!(!error.error.message.contains("secret raw failure"));
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("stream terminal attempt metadata");
        let [occurrence] = metadata.occurrences.as_slice() else {
            panic!("one terminal stream occurrence expected for {name}");
        };
        assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
        assert_eq!(occurrence.diagnostic_code.as_deref(), Some(expected_code));
        assert!(!occurrence.retry_scheduled);
        requests
            .recv_timeout(Duration::from_secs(1))
            .expect("stream request was sent");
    }
}

#[test]
fn openai_responses_stream_rejects_oversized_body_and_ignores_tool_argument_deltas() {
    let body = format!("data: {}\n\n", "x".repeat(8 * 1024 * 1024 + 1));
    let chunks = body
        .as_bytes()
        .chunks(64 * 1024)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let provider = OpenAiProvider::new(provider_config_with_base_url(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_oversized",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let error = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect_err("oversized stream must fail closed");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_stream_too_large")
    );
    let metadata = error
        .provider_attempt_metadata
        .as_ref()
        .expect("oversized stream attempt metadata");
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one oversized stream occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
    assert_eq!(
        occurrence.error_stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    assert!(!occurrence.retry_scheduled);
    requests
        .recv_timeout(Duration::from_secs(1))
        .expect("oversized stream request");

    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "response_tool_stream",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_read",
                "name": "read",
                "arguments": "{\"path\":\"README.md\"}"
            }]
        }
    });
    let tool_body = format!(
        "event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{{\\\"path\\\":\\\"README.md\\\"}}\"}}\n\nevent: response.completed\ndata: {completed}\n\n"
    );
    let chunks = tool_body
        .as_bytes()
        .chunks(5)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let provider = OpenAiProvider::new(provider_config_with_base_url(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_tool_stream",
        vec![ModelMessage::text(ModelRole::User, "call read")],
    );
    let mut events = Vec::new();
    let response = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect("final function call envelope");
    assert!(events.is_empty());
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(
        response.tool_calls[0].raw_arguments,
        r#"{"path":"README.md"}"#
    );
    requests
        .recv_timeout(Duration::from_secs(1))
        .expect("tool stream request");
}

#[test]
fn openai_chat_streaming_is_explicitly_unsupported() {
    let provider = OpenAiProvider::new(provider_config_with_base_url(
        "http://127.0.0.1:1/v1/chat/completions".to_string(),
    ))
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_chat_stream",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let error = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect_err("Chat streaming must be unsupported");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_streaming_unsupported")
    );
}

#[test]
fn streaming_capability_is_bound_to_the_selected_protocol() {
    assert_eq!(
        ProviderStreamingCapability::for_protocol(ProviderApiProtocol::OpenAiResponses),
        ProviderStreamingCapability::OutputTextDelta
    );
    for protocol in [
        ProviderApiProtocol::Declared,
        ProviderApiProtocol::OpenAiChatCompletions,
    ] {
        assert_eq!(
            ProviderStreamingCapability::for_protocol(protocol),
            ProviderStreamingCapability::Unsupported
        );
    }

    let provider = OpenAiProvider::new(provider_config_with_base_url(
        "http://127.0.0.1:1/v1/responses".to_string(),
    ))
    .expect("provider");
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::OpenAiResponses),
        ProviderStreamingCapability::OutputTextDelta
    );
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::OpenAiChatCompletions),
        ProviderStreamingCapability::Unsupported
    );
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::Declared),
        ProviderStreamingCapability::Unsupported
    );
}

#[test]
fn openai_responses_stream_retries_before_but_not_after_first_text_delta() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stream retry provider");
    let address = listener
        .local_addr()
        .expect("stream retry provider address");
    let server = thread::spawn(move || {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept stream retry request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            read_provider_request(&mut reader);
            if attempt == 0 {
                let body = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len() + 16,
                    body
                )
                .expect("write truncated stream retry body");
            } else {
                let delta = serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "done"
                });
                let completed = serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "response_retry",
                        "object": "response",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "done"}]
                        }]
                    }
                });
                let body = format!(
                    "event: response.output_text.delta\ndata: {delta}\n\nevent: response.completed\ndata: {completed}\n\n"
                );
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                )
                .expect("write successful stream retry body");
            }
        }
    });
    let provider = OpenAiProvider::new(provider_config_with_base_url(format!(
        "http://{address}/v1/responses"
    )))
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_retry_before_delta",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();
    let response = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect("retry before delta");
    let metadata = response
        .provider_attempt_metadata
        .expect("stream retry metadata");
    assert_eq!(metadata.attempt_count, 2);
    assert_eq!(metadata.retry_count, 1);
    assert_eq!(metadata.occurrences.len(), 2);
    assert_eq!(metadata.occurrences[0].attempt_index, 1);
    assert_eq!(
        metadata.occurrences[0].terminal_status,
        ProviderAttemptStatus::Error
    );
    assert!(metadata.occurrences[0].retry_scheduled);
    assert!(
        metadata.occurrences[0]
            .time_to_first_text_delta_ms
            .is_none()
    );
    assert_eq!(metadata.occurrences[1].attempt_index, 2);
    assert_eq!(
        metadata.occurrences[1].terminal_status,
        ProviderAttemptStatus::Ok
    );
    assert!(
        metadata.occurrences[1]
            .time_to_first_text_delta_ms
            .is_some()
    );
    assert_eq!(events.len(), 1);
    server.join().expect("join stream retry server");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stream no-retry provider");
    let address = listener
        .local_addr()
        .expect("stream no-retry provider address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept stream no-retry request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        read_provider_request(&mut reader);
        let delta = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "partial"
        });
        let body = format!("event: response.output_text.delta\ndata: {delta}\n\n");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len() + 16
        )
        .expect("write truncated post-delta body");
    });
    let provider = OpenAiProvider::new(provider_config_with_base_url(format!(
        "http://{address}/v1/responses"
    )))
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_no_retry_after_delta",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();
    let error = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect_err("post-delta body failure");
    let metadata = error
        .provider_attempt_metadata
        .expect("post-delta metadata");
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one post-delta occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
    assert!(occurrence.time_to_first_text_delta_ms.is_some());
    assert!(!occurrence.retry_scheduled);
    assert_eq!(events.len(), 1);
    server.join().expect("join stream no-retry server");
}

#[test]
fn openai_responses_stream_cancellation_reaches_inflight_body_read() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stream cancellation provider");
    let address = listener
        .local_addr()
        .expect("stream cancellation provider address");
    let (started_tx, started_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .expect("accept stream cancellation request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        read_provider_request(&mut reader);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
        )
        .expect("write stream cancellation headers");
        stream.flush().expect("flush stream cancellation headers");
        started_tx
            .send(())
            .expect("signal stream cancellation start");
        thread::sleep(Duration::from_millis(500));
    });
    let provider = OpenAiProvider::new(provider_config_with_base_url(format!(
        "http://{address}/v1/responses"
    )))
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_cancel",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let cancellation = singularity_core::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        result_tx
            .send(provider.complete_stream(&request, &worker_cancellation, &mut |_| {}))
            .expect("send cancellation result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("stream request started");
    cancellation.cancel();
    let error = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("stream cancellation was bounded")
        .expect_err("stream cancellation");
    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
    let metadata = error.provider_attempt_metadata.expect("cancel metadata");
    assert_eq!(metadata.attempt_count, 1);
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one cancelled stream occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Cancelled);
    assert_eq!(
        occurrence.error_category,
        Some(ModelErrorCategory::Cancelled)
    );
    assert_eq!(occurrence.error_stage, Some(ProviderErrorStage::Cancelled));
    assert!(!occurrence.retry_scheduled);
    worker.join().expect("join stream cancellation worker");
    server.join().expect("join stream cancellation server");
}

#[test]
fn openai_tool_history_finalization_reuses_negotiated_protocol_without_tools() {
    let (base_url, requests) = finalization_protocol_server();
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();
    let negotiation = provider
        .negotiate_tool_capabilities(&ModelPreferences::default(), &cancellation)
        .expect("Responses capability negotiation");
    assert_eq!(
        negotiation.metadata.api_protocol,
        ProviderApiProtocol::OpenAiResponses
    );

    let request = history_only_finalization_request("request_finalization");

    let response = provider
        .complete(&request, &cancellation)
        .expect("tool-history finalization response");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("done")
    );

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured finalization requests");
    assert_eq!(captured.len(), 3);
    assert!(
        captured
            .iter()
            .all(|(path, _)| path.contains("/v1/responses"))
    );
    let final_payload: serde_json::Value =
        serde_json::from_str(&captured[2].1).expect("finalization request JSON");
    let input = final_payload["input"]
        .as_array()
        .expect("Responses finalization input");
    assert!(input.iter().any(|item| item["type"] == "function_call"));
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
    assert!(final_payload.get("tools").is_none());
    assert_eq!(final_payload["reasoning"]["effort"], "none");
}

#[test]
fn openai_chat_tool_history_finalization_disables_reasoning_without_tools() {
    let (base_url, requests) = reasoning_stabilization_probe_server(
        "HTTP/1.1 200 OK",
        PROBE_STRICT_PARALLEL_RESPONSE,
        ACTUAL_DONE_RESPONSE,
        1,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");

    let response = provider
        .complete(
            &history_only_finalization_request("chat_request_finalization"),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Chat tool-history finalization response");
    assert_eq!(response.status, ModelTurnStatus::Success);

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Chat finalization requests");
    assert_eq!(captured.len(), 4);
    let payload: serde_json::Value =
        serde_json::from_str(captured.last().expect("Chat finalization request"))
            .expect("Chat finalization request JSON");
    assert!(payload.get("tools").is_none());
    assert_eq!(payload["thinking"]["type"], "disabled");
}

#[test]
fn openai_chat_tool_history_finalization_rejects_reasoning_content() {
    let (base_url, requests) = reasoning_stabilization_probe_server(
        "HTTP/1.1 200 OK",
        PROBE_STRICT_PARALLEL_RESPONSE,
        CHAT_HISTORY_REASONING_RESPONSE,
        1,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");

    let error = provider
        .complete(
            &history_only_finalization_request("chat_request_reasoning"),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("Chat history-only reasoning must fail closed");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_tool_reasoning_mode_not_honored")
    );
    assert!(
        error
            .error
            .validation_errors
            .contains(&"tool_reasoning_disable_not_honored".to_string())
    );
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Chat reasoning boundary requests");
    assert_eq!(captured.len(), 4);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(captured.last().expect("actual request JSON"))
            .expect("actual request JSON")["thinking"]["type"],
        "disabled"
    );
}

#[test]
fn openai_responses_text_tool_envelope_remains_invalid_and_unexecuted() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_text_tool_call",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": "<tool_call><function=read><parameter=path>Cargo.toml</parameter></function></tool_call>"
            }]
        }],
        "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
    }));
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 2),
            &singularity_core::CancellationToken::new(),
        )
        .expect("invalid provider response remains observable");

    assert_eq!(response.status, ModelTurnStatus::Invalid);
    assert!(response.tool_calls.is_empty());
    assert_eq!(
        response.validation.expect("response validation").errors,
        vec!["text_tool_call_envelope_not_supported"]
    );
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Responses requests")
            .len(),
        3
    );
}

#[test]
fn openai_provider_falls_back_from_unsupported_responses_endpoint_to_chat() {
    let (base_url, requests) = responses_to_chat_fallback_server();
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();
    let negotiation = provider
        .negotiate_tool_capabilities(&ModelPreferences::default(), &cancellation)
        .expect("Chat fallback negotiation");

    assert_eq!(
        negotiation.metadata.api_protocol,
        ProviderApiProtocol::OpenAiChatCompletions
    );
    assert_eq!(negotiation.metadata.profile_attempts, 2);
    assert_eq!(negotiation.metadata.fallback_count, 1);

    let response = provider
        .complete(&capability_test_request(None, false, 2), &cancellation)
        .expect("Chat fallback completion");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls[0].tool_name, "read");

    let paths = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured protocol fallback paths");
    assert_eq!(paths.len(), 4);
    assert!(paths[0].contains("/v1/responses"));
    assert!(
        paths[1..]
            .iter()
            .all(|path| path.contains("/v1/chat/completions"))
    );
}

#[test]
fn openai_provider_does_not_fallback_protocol_on_authentication_failure() {
    let (base_url, request_path) =
        protocol_status_server("HTTP/1.1 401 Unauthorized", r#"{"error":"unauthorized"}"#);
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let error = provider
        .negotiate_tool_capabilities(
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("authentication failure must terminate protocol negotiation");

    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.http_status, Some(401));
    assert_eq!(
        error
            .capability_metadata
            .expect("capability metadata")
            .api_protocol,
        ProviderApiProtocol::OpenAiResponses
    );
    assert!(
        request_path
            .recv_timeout(Duration::from_secs(1))
            .expect("captured protocol request")
            .contains("/v1/responses")
    );
}

#[test]
fn openai_provider_does_not_retry_or_fallback_malformed_responses_output() {
    let (base_url, request_path) = protocol_status_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"bad_response","status":"completed","output":[{"type":"unsupported"}]}"#,
    );
    let provider = OpenAiProvider::new(provider_auto_test_config(base_url)).expect("provider");
    let error = provider
        .negotiate_tool_capabilities(
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("malformed Responses output must terminate negotiation");

    assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert_eq!(
        error.error.validation_errors,
        vec!["responses_output_item_unsupported"]
    );
    let metadata = error.capability_metadata.expect("capability metadata");
    assert_eq!(metadata.api_protocol, ProviderApiProtocol::OpenAiResponses);
    assert_eq!(metadata.profile_attempts, 1);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 1);
    assert!(
        request_path
            .recv_timeout(Duration::from_secs(1))
            .expect("captured protocol request")
            .contains("/v1/responses")
    );
}

#[test]
fn provider_limits_default_and_configured_capabilities_are_explicit() {
    let default_config = OpenAiProviderConfig::from_env(|name| match name {
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        _ => None,
    })
    .expect("provider config");
    assert_eq!(
        default_config.protocol_contract().max_context_tokens,
        DEFAULT_MAX_CONTEXT_TOKENS
    );
    assert_eq!(
        default_config.protocol_contract().max_output_tokens,
        DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert!(
        !default_config
            .protocol_contract()
            .supports_developer_message
    );
    assert!(!default_config.protocol_contract().supports_system_message);
    assert!(!default_config.protocol_contract().supports_json_mode);
    assert!(
        !default_config
            .protocol_contract()
            .supports_parallel_tool_calls
    );
    assert!(
        !default_config
            .protocol_contract()
            .supports_strict_tool_schema
    );

    let configured = OpenAiProviderConfig::from_env(|name| match name {
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        "SINGULARITY_MODEL_CONTEXT_TOKENS" => Some("131072".to_string()),
        "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS" => Some("8192".to_string()),
        _ => None,
    })
    .expect("configured provider");
    let capabilities = configured.protocol_contract();
    assert_eq!(capabilities.max_context_tokens, 131_072);
    assert_eq!(capabilities.max_output_tokens, 8_192);
    assert!(!capabilities.supports_parallel_tool_calls);
    assert!(!capabilities.supports_strict_tool_schema);

    let provider = OpenAiProvider::new(configured).expect("provider");
    assert_eq!(Provider::protocol_contract(&provider), capabilities);
}

#[test]
fn openai_capability_probe_strict_profile_proves_nontrivial_schema_and_arguments() {
    let (base_url, request_rx) = strict_probe_server();
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("strict parallel negotiation");

    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::StrictParallel
    );
    assert!(negotiation.contract.supports_strict_tool_schema);
    assert!(negotiation.contract.supports_developer_message);
    assert!(!negotiation.contract.supports_system_message);
    assert!(!negotiation.contract.supports_json_mode);
    assert!(negotiation.contract.supports_parallel_tool_calls);
    assert_eq!(negotiation.contract.max_tools_per_request, 8);
    let request_bodies = request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured strict capability requests");
    assert_eq!(request_bodies.len(), 2);
    let request: serde_json::Value =
        serde_json::from_str(&request_bodies[0]).expect("strict capability request JSON");
    assert_eq!(request["tools"].as_array().map(Vec::len), Some(8));
    let parameters = &request["tools"][0]["function"]["parameters"];
    assert_eq!(request["messages"][0]["role"], "developer");
    assert_eq!(request["messages"][1]["role"], "user");
    let branches = parameters["oneOf"]
        .as_array()
        .expect("strict probe top-level oneOf branches");
    assert_eq!(branches.len(), 2);
    let parameters = &branches[0];
    assert_eq!(
        parameters["required"],
        serde_json::json!(["probe", "values"])
    );
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(
        parameters["properties"]["probe"]["const"],
        "schema_sentinel_alpha"
    );
    assert_eq!(parameters["properties"]["values"]["type"], "array");
    assert_eq!(parameters["properties"]["values"]["minItems"], 2);
    assert_eq!(parameters["properties"]["values"]["maxItems"], 2);
    let instruction = request["messages"][1]["content"]
        .as_str()
        .expect("strict probe instruction");
    assert_eq!(
        instruction,
        "First call singularity_capability_probe_a and singularity_capability_probe_b once each. After both tool results, call singularity_capability_probe_a once more."
    );
    assert!(!instruction.contains("schema_sentinel_alpha"));
    assert!(!instruction.contains("7"));
    assert!(!instruction.contains("values"));
    for tool in request["tools"].as_array().expect("strict probe tools") {
        let description = tool["function"]["description"]
            .as_str()
            .expect("strict probe description");
        assert_eq!(
            description,
            "Fixed capability probe tool; no external side effect."
        );
        assert!(!description.contains("schema_sentinel_alpha"));
        assert!(!description.contains("7"));
    }
    assert_eq!(request["tools"][0]["function"]["strict"], true);
    assert!(!negotiation.contract.supports_required_tool_choice);
    let continuation: serde_json::Value = serde_json::from_str(&request_bodies[1])
        .expect("strict capability continuation request JSON");
    assert_eq!(continuation["tool_choice"], "auto");
    assert_eq!(continuation["parallel_tool_calls"], false);
    assert_eq!(continuation["messages"][2]["role"], "assistant");
    assert_eq!(
        continuation["messages"][2]["tool_calls"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert_eq!(continuation["messages"][3]["role"], "tool");
    assert_eq!(continuation["messages"][3].get("name"), None);
    assert_eq!(continuation["messages"][3]["tool_call_id"], "probe_call_a");
    assert_eq!(continuation["messages"][4]["role"], "tool");
    assert_eq!(continuation["messages"][4].get("name"), None);
    assert_eq!(continuation["messages"][4]["tool_call_id"], "probe_call_b");
    assert_eq!(negotiation.metadata.probe_attempt_metadata.attempt_count, 2);
    assert_eq!(
        negotiation
            .metadata
            .probe_attempt_metadata
            .occurrences
            .iter()
            .map(|occurrence| (
                occurrence.operation_phase,
                occurrence.attempt_index,
                occurrence.terminal_status,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                ProviderAttemptOperationPhase::CapabilityProbe,
                1,
                ProviderAttemptStatus::Ok,
            ),
            (
                ProviderAttemptOperationPhase::CapabilityProbe,
                2,
                ProviderAttemptStatus::Ok,
            ),
        ]
    );
    let cached = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("cached strict capability negotiation");
    assert!(cached.metadata.cache_hit);
    assert_eq!(cached.metadata.profile_attempts, 0);
    assert_eq!(cached.metadata.fallback_count, 0);
    assert_eq!(cached.metadata.probe_usage, ModelUsage::default());
    assert_eq!(
        cached.metadata.probe_attempt_metadata,
        ProviderAttemptMetadata::default()
    );
}

#[test]
fn openai_capability_negotiation_stabilizes_reasoning_content_tool_calls() {
    let (base_url, requests) = reasoning_stabilization_probe_server(
        "HTTP/1.1 200 OK",
        PROBE_STRICT_PARALLEL_RESPONSE,
        ACTUAL_DONE_RESPONSE,
        1,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("reasoning tool calls stabilized");

    assert_eq!(
        negotiation.contract.tool_reasoning_mode,
        ProviderToolReasoningMode::DisabledForToolCalls
    );
    assert_eq!(negotiation.metadata.profile_attempts, 1);
    assert_eq!(negotiation.metadata.fallback_count, 0);
    assert_eq!(negotiation.metadata.probe_usage.total_tokens, 9);
    assert_eq!(negotiation.metadata.probe_attempt_metadata.attempt_count, 3);

    provider
        .complete(
            &capability_test_request(None, true, 2),
            &singularity_core::CancellationToken::new(),
        )
        .expect("actual completion uses negotiated reasoning mode");
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured reasoning negotiation requests");
    assert_eq!(captured.len(), 4);
    let captured = captured
        .iter()
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(request).expect("captured request JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(captured[0].get("thinking"), None);
    assert_eq!(captured[1]["thinking"]["type"], "disabled");
    assert_eq!(captured[2]["thinking"]["type"], "disabled");
    assert_eq!(captured[3]["thinking"]["type"], "disabled");
    assert!(
        captured
            .iter()
            .all(|request| request["tool_choice"] == "auto")
    );
}

#[test]
fn openai_provider_rejects_tool_reasoning_when_negotiated_disable_is_not_honored() {
    let (base_url, requests) = reasoning_stabilization_probe_server(
        "HTTP/1.1 200 OK",
        PROBE_STRICT_PARALLEL_RESPONSE,
        ACTUAL_TOOL_REASONING_RESPONSE,
        1,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("reasoning disable negotiation");
    assert_eq!(
        negotiation.contract.tool_reasoning_mode,
        ProviderToolReasoningMode::DisabledForToolCalls
    );

    let error = provider
        .complete(
            &capability_test_request(None, true, 2),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("unreplayable tool reasoning must fail closed");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_tool_reasoning_mode_not_honored")
    );
    assert!(
        error
            .error
            .validation_errors
            .contains(&"tool_reasoning_disable_not_honored".to_string())
    );
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured reasoning boundary requests");
    assert_eq!(captured.len(), 4);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&captured[3]).expect("actual request JSON")["thinking"]
            ["type"],
        "disabled"
    );
}

#[test]
fn openai_capability_negotiation_rejects_unstable_reasoning_tool_mode() {
    let (base_url, requests) = reasoning_stabilization_probe_server(
        "HTTP/1.1 400 Bad Request",
        "{}",
        ACTUAL_DONE_RESPONSE,
        0,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("unsupported reasoning control must fail closed");

    assert_eq!(error.error.kind, ModelErrorKind::UnknownProviderError);
    assert_eq!(error.error.code.as_deref(), Some("provider_http_status"));
    assert_eq!(error.error.stage, Some(ProviderErrorStage::ResponseStatus));
    assert_eq!(error.error.http_status, Some(400));
    assert!(
        error
            .error
            .validation_errors
            .iter()
            .any(|error| error == "tool_reasoning_disable_unsupported")
    );
    let metadata = error
        .capability_metadata
        .expect("reasoning failure capability metadata");
    assert_eq!(metadata.profile_attempts, 1);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 2);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured failed reasoning probes");
    assert_eq!(captured.len(), 2);
}

#[test]
fn openai_capability_probe_negotiates_tool_definition_capacity_and_caches_it() {
    let (base_url, requests, _started) = delayed_probe_server(
        vec![("HTTP/1.1 200 OK", PROBE_STRICT_PARALLEL_RESPONSE)],
        Duration::ZERO,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();

    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("bounded tool-definition negotiation");
    let cached = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("cached tool-definition negotiation");

    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::StrictParallel
    );
    assert!(negotiation.contract.supports_parallel_tool_calls);
    assert_eq!(negotiation.contract.max_tools_per_request, 8);
    assert_eq!(negotiation.metadata.profile_attempts, 1);
    assert_eq!(negotiation.metadata.fallback_count, 0);
    assert!(cached.metadata.cache_hit);
    assert_eq!(cached.contract, negotiation.contract);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured tool-definition probes");
    let tool_counts = captured
        .iter()
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(request).expect("probe JSON")["tools"]
                .as_array()
                .map(Vec::len)
                .expect("probe tools")
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_counts, vec![8, 8]);
}

#[test]
fn openai_capability_probe_strict_constraint_mismatch_falls_back_to_non_strict() {
    for (case_name, bad_arguments) in [
        ("const", r#"{"probe":"wrong_probe","values":[7,7]}"#),
        (
            "array",
            r#"{"probe":"schema_sentinel_alpha","values":{"value":7}}"#,
        ),
    ] {
        let (base_url, done) = strict_constraint_mismatch_probe_server(bad_arguments);
        let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
        let negotiation = Provider::negotiate_tool_capabilities(
            &provider,
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
        .unwrap_or_else(|error| panic!("{case_name} mismatch should fall back: {error:?}"));

        assert_eq!(
            negotiation.metadata.profile,
            ProviderCapabilityProfile::NonStrictParallel
        );
        assert!(!negotiation.contract.supports_strict_tool_schema);
        assert!(negotiation.contract.supports_parallel_tool_calls);
        assert_eq!(negotiation.contract.max_tools_per_request, 8);
        assert_eq!(negotiation.metadata.profile_attempts, 3);
        assert_eq!(negotiation.metadata.fallback_count, 2);
        done.recv_timeout(Duration::from_secs(1))
            .expect("strict constraint probe fallback completed");
    }
}

#[test]
fn openai_capability_probe_non_strict_single_uses_auto_tool_choice() {
    let (base_url, requests, _started) = delayed_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_NON_STRICT_SINGLE_RESPONSE),
        ],
        Duration::ZERO,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("non-strict single negotiation");

    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::NonStrictSingle
    );
    assert!(!negotiation.contract.supports_strict_tool_schema);
    assert_eq!(negotiation.contract.max_tools_per_request, 8);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured non-strict probe requests");
    let single_request: serde_json::Value =
        serde_json::from_str(captured.last().expect("single probe request"))
            .expect("single probe JSON");
    assert_eq!(single_request["tool_choice"], "auto");
    assert_eq!(single_request["tools"].as_array().map(Vec::len), Some(8));
    assert_eq!(single_request["tools"][0]["function"].get("strict"), None);
}

#[test]
fn openai_capability_probe_fails_closed_when_direct_tools_are_unsupported() {
    let (base_url, requests) = direct_only_probe_server();
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");

    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("unsupported direct tools must not become a negotiated capability");

    assert_eq!(error.error.kind, ModelErrorKind::UnknownProviderError);
    assert_eq!(error.error.code.as_deref(), Some("provider_http_status"));
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured direct capability requests");
    assert_eq!(captured.len(), 4);
    for request in captured {
        let request: serde_json::Value = serde_json::from_str(&request).expect("probe JSON");
        let names = request["tools"]
            .as_array()
            .expect("probe tools")
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>();
        assert!(!names.is_empty());
        assert!(
            names
                .iter()
                .all(|name| name.starts_with("singularity_capability_probe_"))
        );
    }
}

#[test]
fn openai_capability_probe_preserves_strict_when_parallel_is_unproven() {
    let (base_url, requests) = configurable_probe_server(
        vec![("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE)],
        ACTUAL_DONE_RESPONSE,
        1,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("strict single negotiation");

    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::StrictSingle
    );
    assert!(negotiation.contract.supports_strict_tool_schema);
    assert!(!negotiation.contract.supports_parallel_tool_calls);
    assert_eq!(negotiation.metadata.profile_attempts, 1);
    assert_eq!(negotiation.metadata.fallback_count, 0);

    let response = provider
        .complete(
            &capability_test_request(None, true, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("actual completion after cached negotiation");
    assert_eq!(response.status, ModelTurnStatus::Success);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured actual request");
    let actual: serde_json::Value = serde_json::from_str(&captured[0]).expect("actual JSON");
    assert_eq!(actual["model"], "gpt-test");
    assert_eq!(actual["parallel_tool_calls"], false);
    assert_eq!(actual["tools"][0]["function"]["strict"], true);
}

#[test]
fn openai_capability_probe_failed_single_flight_shares_typed_outcome() {
    let (base_url, requests, _started) = delayed_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 422 Unprocessable Entity", "{}"),
        ],
        Duration::from_millis(100),
    );
    let provider = Arc::new(OpenAiProvider::new(provider_test_config(base_url)).expect("provider"));
    let start = Arc::new(Barrier::new(4));
    let handles = (0..4)
        .map(|_| {
            let provider = Arc::clone(&provider);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                Provider::negotiate_tool_capabilities(
                    provider.as_ref(),
                    &ModelPreferences::default(),
                    &singularity_core::CancellationToken::new(),
                )
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("join capability caller"))
        .collect::<Vec<_>>();

    let first_error = results[0].as_ref().expect_err("probe must fail");
    for result in results.iter().skip(1) {
        assert_eq!(result.as_ref().expect_err("probe must fail"), first_error);
    }
    assert_eq!(first_error.error.kind, ModelErrorKind::UnknownProviderError);
    assert_eq!(first_error.error.http_status, Some(422));
    assert_eq!(
        first_error
            .capability_metadata
            .as_ref()
            .expect("capability metadata")
            .profile_attempts,
        4
    );

    let captured = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("captured capability probes");
    assert_eq!(captured.len(), 4, "one shared probe round is required");
}

#[test]
fn openai_capability_probe_waiter_cancellation_is_caller_local() {
    let (base_url, requests, started) = delayed_probe_server(
        vec![("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE)],
        Duration::from_millis(250),
    );
    let provider = Arc::new(OpenAiProvider::new(provider_test_config(base_url)).expect("provider"));
    let owner_provider = Arc::clone(&provider);
    let (owner_tx, owner_rx) = mpsc::channel();
    let owner = thread::spawn(move || {
        owner_tx
            .send(Provider::negotiate_tool_capabilities(
                owner_provider.as_ref(),
                &ModelPreferences::default(),
                &singularity_core::CancellationToken::new(),
            ))
            .expect("send owner result");
    });
    started
        .recv_timeout(Duration::from_secs(1))
        .expect("owner probe started");

    let waiter_cancellation = singularity_core::CancellationToken::new();
    let waiter_provider = Arc::clone(&provider);
    let waiter_token = waiter_cancellation.clone();
    let (waiter_tx, waiter_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        waiter_tx
            .send(Provider::negotiate_tool_capabilities(
                waiter_provider.as_ref(),
                &ModelPreferences::default(),
                &waiter_token,
            ))
            .expect("send waiter result");
    });
    thread::sleep(Duration::from_millis(50));
    waiter_cancellation.cancel();

    let waiter_error = waiter_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("waiter cancellation was bounded")
        .expect_err("waiter must be cancelled locally");
    assert_eq!(waiter_error.error.kind, ModelErrorKind::Cancelled);
    assert!(
        owner_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owner result")
            .is_ok()
    );
    owner.join().expect("join owner");
    waiter.join().expect("join waiter");

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured owner capability probe");
    assert_eq!(
        captured.len(),
        2,
        "waiter cancellation must not cancel owner"
    );
}

#[test]
fn openai_capability_probe_owner_cancellation_allows_waiter_takeover() {
    let (base_url, requests, started) = delayed_probe_server(
        vec![
            ("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE),
            ("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE),
        ],
        Duration::from_millis(250),
    );
    let provider = Arc::new(OpenAiProvider::new(provider_test_config(base_url)).expect("provider"));
    let owner_cancellation = singularity_core::CancellationToken::new();
    let owner_token = owner_cancellation.clone();
    let owner_provider = Arc::clone(&provider);
    let (owner_tx, owner_rx) = mpsc::channel();
    let owner = thread::spawn(move || {
        owner_tx
            .send(Provider::negotiate_tool_capabilities(
                owner_provider.as_ref(),
                &ModelPreferences::default(),
                &owner_token,
            ))
            .expect("send owner result");
    });
    started
        .recv_timeout(Duration::from_secs(1))
        .expect("owner probe started");

    let waiter_provider = Arc::clone(&provider);
    let (waiter_tx, waiter_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        waiter_tx
            .send(Provider::negotiate_tool_capabilities(
                waiter_provider.as_ref(),
                &ModelPreferences::default(),
                &singularity_core::CancellationToken::new(),
            ))
            .expect("send waiter result");
    });
    thread::sleep(Duration::from_millis(50));
    owner_cancellation.cancel();

    let owner_error = owner_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("owner cancellation was bounded")
        .expect_err("owner must be cancelled");
    assert_eq!(owner_error.error.kind, ModelErrorKind::Cancelled);
    assert!(
        waiter_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter takeover result")
            .is_ok()
    );
    owner.join().expect("join owner");
    waiter.join().expect("join waiter");

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured owner and takeover probes");
    assert_eq!(
        captured.len(),
        3,
        "waiter must take over after owner cancellation"
    );
}

#[test]
fn openai_capability_probe_final_text_response_is_typed_and_preserves_probe_metadata() {
    let (base_url, requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_TEXT_RESPONSE),
        ],
        ACTUAL_DONE_RESPONSE,
        0,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("text pseudo-call must not prove native tools");

    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert!(
        error
            .error
            .validation_errors
            .contains(&"capability_probe_native_tool_calls_missing".to_string())
    );
    let metadata = error.capability_metadata.expect("probe metadata");
    assert_eq!(metadata.profile, ProviderCapabilityProfile::NonStrictSingle);
    assert_eq!(metadata.profile_attempts, 4);
    assert_eq!(metadata.fallback_count, 3);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 4);
    assert!(requests.recv_timeout(Duration::from_secs(1)).is_ok());
}

#[test]
fn openai_capability_probe_requires_native_tool_calls_after_tool_results_before_caching() {
    let (base_url, requests) = multi_turn_probe_recovery_server();
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();

    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect_err("a text continuation must not prove multi-turn native tools");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert!(
        error
            .error
            .validation_errors
            .contains(&"capability_probe_multi_turn_tool_calls_missing".to_string())
    );
    let failed_metadata = error.capability_metadata.expect("failed probe metadata");
    assert_eq!(failed_metadata.profile_attempts, 4);
    assert_eq!(failed_metadata.probe_attempt_metadata.attempt_count, 5);

    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("a failed multi-turn probe must not poison or populate the success cache");
    assert!(!negotiation.metadata.cache_hit);
    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::StrictParallel
    );
    assert_eq!(negotiation.metadata.probe_attempt_metadata.attempt_count, 2);
    let cached = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("successful multi-turn probe is cached");
    assert!(cached.metadata.cache_hit);

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured failed and successful multi-turn probes");
    assert_eq!(captured.len(), 7);
    for continuation_index in [4, 6] {
        let request: serde_json::Value =
            serde_json::from_str(&captured[continuation_index]).expect("continuation JSON");
        assert_eq!(request["tool_choice"], "auto");
        assert!(
            request["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool"))
        );
    }
}

#[test]
fn openai_capability_probe_auth_failure_does_not_fallback() {
    let (base_url, requests) = configurable_probe_server(
        vec![("HTTP/1.1 401 Unauthorized", "{}")],
        ACTUAL_DONE_RESPONSE,
        0,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("auth failure must be preserved");

    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.http_status, Some(401));
    let metadata = error.capability_metadata.expect("probe metadata");
    assert_eq!(metadata.profile_attempts, 1);
    assert_eq!(metadata.fallback_count, 0);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 1);
    assert!(requests.recv_timeout(Duration::from_secs(1)).is_ok());
}

#[test]
fn openai_capability_probe_all_profile_rejections_preserve_provider_cause() {
    let (base_url, requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 422 Unprocessable Entity", "{}"),
        ],
        ACTUAL_DONE_RESPONSE,
        0,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("profile rejection cause must be preserved");

    assert_eq!(error.error.kind, ModelErrorKind::UnknownProviderError);
    assert_eq!(error.error.http_status, Some(422));
    assert_eq!(error.error.stage, Some(ProviderErrorStage::ResponseStatus));
    assert!(
        error
            .error
            .validation_errors
            .contains(&"capability_profiles_exhausted".to_string())
    );
    let metadata = error.capability_metadata.expect("probe metadata");
    assert_eq!(metadata.profile_attempts, 4);
    assert_eq!(metadata.fallback_count, 3);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 4);
    assert!(requests.recv_timeout(Duration::from_secs(1)).is_ok());
}

#[test]
fn openai_capability_probe_preserves_structured_validation_errors() {
    let (base_url, _requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_BAD_ARGUMENTS_RESPONSE),
        ],
        ACTUAL_DONE_RESPONSE,
        0,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let error = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("invalid structured arguments must not prove tools");

    assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert!(
        error
            .error
            .validation_errors
            .contains(&"schema_mismatch".to_string())
    );
    assert!(
        error
            .error
            .validation_errors
            .contains(&"tool_call_arguments_must_be_object".to_string())
    );
}

#[test]
fn openai_capability_cache_is_partitioned_by_effective_model_and_shared_by_clones() {
    let (base_url, requests) = multi_model_probe_server(3);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("default model completion");
    provider
        .complete(
            &capability_test_request(Some("model-b"), false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("override model completion");
    let clone = provider.clone();
    clone
        .complete(
            &capability_test_request(Some("model-b"), false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("same override model cache hit");
    let cache_hit = Provider::negotiate_tool_capabilities(
        &clone,
        &ModelPreferences {
            model_name: Some("model-b".to_string()),
            ..ModelPreferences::default()
        },
        &singularity_core::CancellationToken::new(),
    );
    assert!(
        cache_hit
            .expect("same model cache metadata")
            .metadata
            .cache_hit
    );

    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured model requests");
    let probe_count = captured
        .iter()
        .filter(|request| request.contains("singularity_capability_probe"))
        .count();
    assert_eq!(
        probe_count, 4,
        "different models probe independently; clone shares cache"
    );
    let models = captured
        .iter()
        .filter(|request| !request.contains("singularity_capability_probe"))
        .map(|request| {
            serde_json::from_str::<serde_json::Value>(request).expect("actual JSON")["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(models, vec!["gpt-test", "model-b", "model-b"]);
}

#[test]
fn openai_persistent_capability_cache_survives_provider_recreation_without_secrets() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = persistent_probe_server(2);
    let config = provider_test_config(base_url.clone());

    let first = OpenAiProvider::new_with_cache_path(config.clone(), Some(cache_path.clone()))
        .expect("first provider");
    let first_negotiation = Provider::negotiate_tool_capabilities(
        &first,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("first capability probe");
    assert!(!first_negotiation.metadata.cache_hit);
    drop(first);

    let mut changed_key_config = config;
    changed_key_config.api_key = "different-test-key".to_string();
    let second = OpenAiProvider::new_with_cache_path(changed_key_config, Some(cache_path.clone()))
        .expect("recreated provider");
    let cached = Provider::negotiate_tool_capabilities(
        &second,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("persistent capability cache hit");
    assert!(cached.metadata.cache_hit);
    assert_eq!(cached.contract, first_negotiation.contract);
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        2
    );

    let cache_text = std::fs::read_to_string(cache_path).expect("cache file");
    for forbidden in [
        "sk-secret-value",
        "api_key",
        base_url.as_str(),
        "singularity_capability_probe",
        "schema_sentinel",
        "http://",
    ] {
        assert!(!cache_text.contains(forbidden), "cache leaked {forbidden}");
    }
}

#[test]
fn openai_persistent_capability_cache_replaces_existing_file_for_distinct_key() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = persistent_probe_server(4);
    let first_config = provider_test_config(base_url);
    let first = OpenAiProvider::new_with_cache_path(first_config.clone(), Some(cache_path.clone()))
        .expect("first provider");
    Provider::negotiate_tool_capabilities(
        &first,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("first capability probe");
    drop(first);

    let mut second_config = first_config;
    second_config.max_output_tokens -= 1;
    let second = OpenAiProvider::new_with_cache_path(second_config, Some(cache_path.clone()))
        .expect("second provider");
    Provider::negotiate_tool_capabilities(
        &second,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("second capability probe");

    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        4
    );
    let cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache_path).expect("read cache"))
            .expect("cache remains JSON");
    assert_eq!(cache["records"].as_array().map(Vec::len), Some(2));
    assert_ne!(
        cache["records"][0]["key"]["max_output_tokens"],
        cache["records"][1]["key"]["max_output_tokens"]
    );
}

#[test]
fn openai_persistent_capability_cache_misses_expired_unknown_and_invalid_records() {
    for mutation in [
        "expired",
        "future",
        "corrupt",
        "unknown",
        "invalid_contract",
        "overclaimed_contract",
        "model",
        "endpoint",
        "protocol",
        "limits",
        "adapter",
        "probe_contract",
    ] {
        let directory = tempdir().expect("persistent cache directory");
        let cache_path = directory.path().join("provider-capability-cache.json");
        let (base_url, requests) = persistent_probe_server(4);
        let config = provider_test_config(base_url);
        let first = OpenAiProvider::new_with_cache_path(config.clone(), Some(cache_path.clone()))
            .expect("first provider");
        Provider::negotiate_tool_capabilities(
            &first,
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
        .expect("populate persistent cache");
        drop(first);

        if mutation == "corrupt" {
            std::fs::write(&cache_path, b"{not-json").expect("corrupt cache");
        } else {
            let mut cache: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&cache_path).expect("read persistent cache"),
            )
            .expect("valid cache JSON");
            match mutation {
                "expired" => cache["records"][0]["expires_at_unix_seconds"] = serde_json::json!(0),
                "future" => {
                    cache["records"][0]["stored_at_unix_seconds"] = serde_json::json!(u64::MAX)
                }
                "unknown" => cache["records"][0]["unexpected"] = serde_json::json!(true),
                "invalid_contract" => {
                    cache["records"][0]["contract"]["max_tools_per_request"] = serde_json::json!(0)
                }
                "overclaimed_contract" => {
                    cache["records"][0]["contract"]["supports_json_mode"] = serde_json::json!(true)
                }
                "model" => cache["records"][0]["key"]["model_name"] = serde_json::json!("other"),
                "endpoint" => {
                    cache["records"][0]["key"]["endpoint_sha256"] =
                        serde_json::json!("00".repeat(32))
                }
                "protocol" => {
                    cache["records"][0]["key"]["api_protocol"] =
                        serde_json::json!("open_ai_responses")
                }
                "limits" => cache["records"][0]["key"]["max_output_tokens"] = serde_json::json!(1),
                "adapter" => cache["records"][0]["key"]["adapter_version"] = serde_json::json!(2),
                "probe_contract" => {
                    cache["records"][0]["key"]["probe_contract_version"] = serde_json::json!(2)
                }
                _ => unreachable!(),
            }
            std::fs::write(
                &cache_path,
                serde_json::to_vec(&cache).expect("serialize mutated cache"),
            )
            .expect("write mutated cache");
        }

        let second =
            OpenAiProvider::new_with_cache_path(config, Some(cache_path)).expect("second provider");
        let negotiation = Provider::negotiate_tool_capabilities(
            &second,
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
        .expect("cache miss reprobe");
        assert!(!negotiation.metadata.cache_hit, "{mutation} must miss");
        assert_eq!(
            requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
            4
        );
    }
}

#[test]
fn openai_persistent_capability_cache_concurrent_writes_retain_a_valid_record() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = persistent_probe_server(2);
    let config = provider_test_config(base_url);
    let first = OpenAiProvider::new_with_cache_path(config.clone(), Some(cache_path.clone()))
        .expect("first provider");
    let second = OpenAiProvider::new_with_cache_path(config, Some(cache_path.clone()))
        .expect("second provider");
    let barrier = Arc::new(Barrier::new(3));
    let handles = [first, second]
        .into_iter()
        .map(|provider| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Provider::negotiate_tool_capabilities(
                    &provider,
                    &ModelPreferences::default(),
                    &singularity_core::CancellationToken::new(),
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.join().expect("join concurrent cache writer"));
    }
    for result in results {
        result.expect("concurrent capability probe");
    }
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        2
    );
    let cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache_path).expect("read concurrent cache"))
            .expect("concurrent cache remains JSON");
    assert_eq!(cache["records"].as_array().map(Vec::len), Some(1));
}

#[test]
fn openai_persistent_capability_cache_different_keys_probe_in_parallel() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = parallel_persistent_probe_server(4);
    let first = OpenAiProvider::new_with_cache_path(
        provider_test_config(base_url.clone()),
        Some(cache_path.clone()),
    )
    .expect("first provider");
    let mut second_config = provider_test_config(base_url);
    second_config.max_output_tokens -= 1;
    let second = OpenAiProvider::new_with_cache_path(second_config, Some(cache_path))
        .expect("second provider");
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_thread = thread::spawn(move || {
        first_barrier.wait();
        Provider::negotiate_tool_capabilities(
            &first,
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
    });
    let second_barrier = Arc::clone(&barrier);
    let second_thread = thread::spawn(move || {
        second_barrier.wait();
        Provider::negotiate_tool_capabilities(
            &second,
            &ModelPreferences::default(),
            &singularity_core::CancellationToken::new(),
        )
    });
    barrier.wait();
    first_thread
        .join()
        .expect("join first parallel provider")
        .expect("first parallel capability probe");
    second_thread
        .join()
        .expect("join second parallel provider")
        .expect("second parallel capability probe");
    let (maximum, requests) = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("parallel capability requests");
    assert!(maximum >= 2, "different keys must not share a network lock");
    assert_eq!(requests.len(), 4);
}

#[test]
fn openai_persistent_capability_cache_write_failure_does_not_fail_probe() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("cache-directory");
    std::fs::create_dir(&cache_path).expect("cache failure directory");
    let (base_url, requests) = persistent_probe_server(2);
    let provider =
        OpenAiProvider::new_with_cache_path(provider_test_config(base_url), Some(cache_path))
            .expect("provider");
    Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("cache write failure must not fail probe");
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        2
    );
}

#[test]
fn openai_failed_capability_probe_does_not_create_persistent_record() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = direct_only_probe_server();
    let provider = OpenAiProvider::new_with_cache_path(
        provider_test_config(base_url),
        Some(cache_path.clone()),
    )
    .expect("provider");
    Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect_err("failed probe");
    assert!(!cache_path.is_file());
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        4
    );
}

#[test]
fn openai_cancelled_capability_probe_does_not_publish_cache() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests, started) = delayed_probe_server(
        vec![("HTTP/1.1 400 Bad Request", "{}")],
        Duration::from_millis(250),
    );
    let provider = Arc::new(
        OpenAiProvider::new_with_cache_path(
            provider_test_config(base_url),
            Some(cache_path.clone()),
        )
        .expect("provider"),
    );
    let cancellation = singularity_core::CancellationToken::new();
    let worker_provider = Arc::clone(&provider);
    let worker_cancellation = cancellation.clone();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        result_tx
            .send(Provider::negotiate_tool_capabilities(
                worker_provider.as_ref(),
                &ModelPreferences::default(),
                &worker_cancellation,
            ))
            .expect("send cancelled probe result");
    });
    // Cold parallel CI can spend more than one second initializing the HTTP client before accept.
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("probe started");
    cancellation.cancel();
    let error = result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("cancelled probe result")
        .expect_err("cancelled probe must fail");
    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
    assert_eq!(
        error
            .capability_metadata
            .as_ref()
            .expect("cancelled lookup observation")
            .cache_observations
            .len(),
        1
    );
    assert!(
        !cache_path.exists(),
        "cancelled probe must not publish a record"
    );
    worker.join().expect("join cancelled probe");
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        1
    );
}

#[test]
fn openai_cached_capability_rejection_invalidates_persistent_record() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = cached_capability_rejection_server();
    let config = provider_test_config(base_url);
    let first = OpenAiProvider::new_with_cache_path(config.clone(), Some(cache_path.clone()))
        .expect("first provider");
    Provider::negotiate_tool_capabilities(
        &first,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("populate cache");
    let error = first
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("stable capability rejection");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_tool_reasoning_history_unsupported")
    );
    assert!(
        error
            .error
            .validation_errors
            .contains(&"tool_reasoning_content_requires_adapter_history_support".to_string())
    );
    assert!(
        !error
            .capability_metadata
            .as_ref()
            .expect("rejection lookup observations")
            .cache_observations
            .is_empty()
    );
    let invalidated: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&cache_path).expect("read invalidated cache"),
    )
    .expect("invalidated cache remains JSON");
    assert_eq!(invalidated["records"].as_array().map(Vec::len), Some(0));
    drop(first);

    let second =
        OpenAiProvider::new_with_cache_path(config, Some(cache_path)).expect("recreated provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &second,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("reprobe after invalidation");
    assert!(!negotiation.metadata.cache_hit);
    assert_eq!(
        negotiation
            .metadata
            .cache_observations
            .iter()
            .map(|observation| observation.outcome)
            .collect::<Vec<_>>(),
        vec![ProviderCapabilityCacheLookupResult::Miss]
    );
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        5
    );
}

#[test]
fn openai_ordinary_http_400_does_not_invalidate_persistent_record() {
    let directory = tempdir().expect("persistent cache directory");
    let cache_path = directory.path().join("provider-capability-cache.json");
    let (base_url, requests) = ordinary_http_400_server();
    let config = provider_test_config(base_url);
    let first = OpenAiProvider::new_with_cache_path(config.clone(), Some(cache_path.clone()))
        .expect("first provider");
    Provider::negotiate_tool_capabilities(
        &first,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("populate cache");
    let error = first
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("ordinary HTTP 400");
    assert_eq!(error.error.http_status, Some(400));
    assert_ne!(
        error.error.code.as_deref(),
        Some("provider_tool_reasoning_history_unsupported")
    );
    drop(first);

    let second =
        OpenAiProvider::new_with_cache_path(config, Some(cache_path)).expect("second provider");
    let negotiation = Provider::negotiate_tool_capabilities(
        &second,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("ordinary HTTP 400 must preserve the cache");
    assert!(negotiation.metadata.cache_hit);
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(1)).unwrap().len(),
        3
    );
}

#[test]
fn provider_runtime_fingerprint_is_stable_partitioned_and_secret_free() {
    let config = provider_config_with_base_url("https://provider.example/v1".to_string());
    let first = OpenAiProvider::new(config.clone()).expect("first provider");
    let second = OpenAiProvider::new(config.clone()).expect("second provider");
    let first_fingerprint = first.runtime_fingerprint(Some("gpt-test"));
    let second_fingerprint = second.runtime_fingerprint(Some("gpt-test"));
    assert_eq!(first_fingerprint, second_fingerprint);
    assert!(first_fingerprint.negotiation_fingerprint.is_none());

    let mut changed_key_config = config;
    changed_key_config.api_key = "different-test-key".to_string();
    let changed_key_provider =
        OpenAiProvider::new(changed_key_config).expect("changed key provider");
    assert_eq!(
        first_fingerprint,
        changed_key_provider.runtime_fingerprint(Some("gpt-test"))
    );

    let model_fingerprint = first.runtime_fingerprint(Some("other-model"));
    assert_eq!(
        first_fingerprint.provider_fingerprint,
        model_fingerprint.provider_fingerprint
    );
    assert_ne!(
        first_fingerprint.model_fingerprint,
        model_fingerprint.model_fingerprint
    );
    let endpoint_provider = OpenAiProvider::new(provider_config_with_base_url(
        "https://provider.example/other".to_string(),
    ))
    .expect("endpoint provider");
    assert_ne!(
        first_fingerprint.provider_fingerprint,
        endpoint_provider
            .runtime_fingerprint(Some("gpt-test"))
            .provider_fingerprint
    );
    let mut limited_config =
        provider_config_with_base_url("https://provider.example/v1".to_string());
    limited_config.max_context_tokens += 1;
    let limited_provider = OpenAiProvider::new(limited_config).expect("limited provider");
    assert_ne!(
        first_fingerprint.provider_fingerprint,
        limited_provider
            .runtime_fingerprint(Some("gpt-test"))
            .provider_fingerprint
    );

    let negotiation = singularity_model::ProviderProtocolNegotiation {
        contract: ProviderProtocolContract {
            supports_strict_tool_schema: true,
            ..ProviderProtocolContract::default()
        },
        metadata: singularity_model::ProviderCapabilityMetadata {
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            profile: ProviderCapabilityProfile::StrictSingle,
            cache_hit: false,
            profile_attempts: 1,
            fallback_count: 0,
            probe_usage: ModelUsage::default(),
            probe_attempt_metadata: ProviderAttemptMetadata::default(),
            cache_observations: Vec::new(),
        },
    };
    let negotiated = first.runtime_fingerprint_for_negotiation(Some("gpt-test"), &negotiation);
    let negotiated_again =
        second.runtime_fingerprint_for_negotiation(Some("gpt-test"), &negotiation);
    assert_eq!(negotiated, negotiated_again);
    assert_eq!(
        negotiated.provider_fingerprint,
        first_fingerprint.provider_fingerprint
    );
    assert_eq!(
        negotiated.model_fingerprint,
        first_fingerprint.model_fingerprint
    );
    assert!(negotiated.negotiation_fingerprint.is_some());
    let mut other_protocol_negotiation = negotiation.clone();
    other_protocol_negotiation.metadata.api_protocol = ProviderApiProtocol::OpenAiResponses;
    let other_protocol =
        first.runtime_fingerprint_for_negotiation(Some("gpt-test"), &other_protocol_negotiation);
    assert_ne!(
        negotiated.negotiation_fingerprint,
        other_protocol.negotiation_fingerprint
    );
    let serialized = serde_json::to_string(&negotiated).expect("fingerprint JSON");
    for forbidden in [
        "provider.example",
        "sk-secret-value",
        "api_key",
        "singularity_capability_probe",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "fingerprint leaked {forbidden}"
        );
    }
}

#[test]
fn provider_limit_validation_has_bounded_secret_free_errors() {
    for (name, value) in [
        ("SINGULARITY_MODEL_CONTEXT_TOKENS", "zero-limit"),
        ("SINGULARITY_MODEL_CONTEXT_TOKENS", "2000001"),
        ("SINGULARITY_MODEL_MAX_OUTPUT_TOKENS", "256001"),
        ("SINGULARITY_MODEL_MAX_OUTPUT_TOKENS", "not-a-token-limit"),
    ] {
        let result = OpenAiProviderConfig::from_env(|candidate| match candidate {
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
            candidate if candidate == name => Some(value.to_string()),
            _ => None,
        });
        let error = result.expect_err("invalid token limit");

        assert_eq!(error.error.kind, ModelErrorKind::InvalidRequest);
        assert!(error.message.contains(name));
        assert!(!error.message.contains(value));
    }
}

#[test]
fn removed_tool_capability_envs_are_not_read_or_parsed() {
    let config = OpenAiProviderConfig::from_env(|name| {
        assert!(!matches!(
            name,
            "SINGULARITY_MODEL_MAX_TOOL_CALLS" | "SINGULARITY_MODEL_STRICT_TOOL_SCHEMA"
        ));
        match name {
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        }
    })
    .expect("provider configuration");

    assert!(!config.protocol_contract().supports_parallel_tool_calls);
    assert!(!config.protocol_contract().supports_strict_tool_schema);
}

#[test]
fn provider_rejects_output_limit_that_cannot_fit_the_context_window() {
    let error = OpenAiProviderConfig::from_env(|name| match name {
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        "SINGULARITY_MODEL_CONTEXT_TOKENS" => Some("4096".to_string()),
        "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS" => Some("4096".to_string()),
        _ => None,
    })
    .expect_err("inconsistent provider token limits");

    assert_eq!(error.error.kind, ModelErrorKind::InvalidRequest);
    assert!(
        error
            .message
            .contains("SINGULARITY_MODEL_MAX_OUTPUT_TOKENS")
    );
    assert!(error.message.contains("SINGULARITY_MODEL_CONTEXT_TOKENS"));
    assert!(!error.message.contains("sk-secret-value"));
}

#[test]
fn model_request_validation_rejects_output_above_provider_capability() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.model_preferences.max_output_tokens = Some(9);
    let capabilities = ProviderProtocolContract {
        max_output_tokens: 8,
        ..ProviderProtocolContract::default()
    };

    let result = validate_model_request_with_capabilities(&request, Some(&capabilities));

    assert!(!result.valid);
    assert_eq!(
        result.errors,
        vec!["requested_output_tokens_exceed_provider_limit"]
    );
}

#[test]
fn model_request_validation_rejects_parallel_tool_calls_when_provider_does_not_support_them() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "read".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });
    request.tool_choice.max_tool_calls = 2;

    let result = validate_model_request_with_capabilities(
        &request,
        Some(&ProviderProtocolContract::default()),
    );

    assert_eq!(
        result.errors,
        vec!["provider_does_not_support_parallel_tool_calls"]
    );
}

#[test]
fn required_tool_choice_requires_tools_and_negotiated_support() {
    let mut request = ModelTurnRequest::new(
        "request_required",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tool_choice.mode = ToolChoiceMode::Required;

    let missing_tools = validate_model_request(&request);
    assert_eq!(
        missing_tools.errors,
        vec!["required_tool_choice_requires_tools"]
    );

    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "read".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    let unsupported = validate_model_request_with_capabilities(
        &request,
        Some(&ProviderProtocolContract::default()),
    );
    assert_eq!(
        unsupported.errors,
        vec!["provider_does_not_support_required_tool_choice"]
    );

    let supported = ProviderProtocolContract {
        supports_required_tool_choice: true,
        ..ProviderProtocolContract::default()
    };
    assert!(validate_model_request_with_capabilities(&request, Some(&supported)).valid);

    let missing_required_call = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "text")),
        &[],
        &request.tool_choice,
        &["read".to_string()],
        Some(&supported),
    );
    assert_eq!(
        missing_required_call.errors,
        vec!["required_tool_call_missing"]
    );
}

#[test]
fn model_request_validation_rejects_tool_definitions_above_provider_capability() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools = (0..3)
        .map(|index| ModelToolSchema {
            name: format!("tool_{index}"),
            description: "test tool".to_string(),
            parameters_schema: serde_json::json!({"type": "object"}),
        })
        .collect();
    let capabilities = ProviderProtocolContract {
        max_tools_per_request: 2,
        ..ProviderProtocolContract::default()
    };

    let result = validate_model_request_with_capabilities(&request, Some(&capabilities));

    assert_eq!(result.errors, vec!["requested_tools_exceed_provider_limit"]);
}

#[test]
fn model_request_validation_rejects_incompatible_strict_schema_locally() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "read".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });
    request.tool_choice.strict_tool_schema = true;
    let capabilities = ProviderProtocolContract {
        supports_strict_tool_schema: true,
        ..ProviderProtocolContract::default()
    };

    for incompatible_schema in [serde_json::json!({"type": "object"}), serde_json::json!({})] {
        request.tools[0].parameters_schema = incompatible_schema;
        let result = validate_model_request_with_capabilities(&request, Some(&capabilities));
        assert_eq!(result.errors, vec!["strict_tool_schema_incompatible"]);
    }
}

#[test]
fn model_request_validation_rejects_unsupported_declared_capabilities() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::Developer, "instructions")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "read".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    request.model_preferences.json_mode = true;
    request.tool_choice.strict_tool_schema = true;
    let capabilities = ProviderProtocolContract {
        supports_tools: false,
        supports_json_mode: false,
        supports_developer_message: false,
        ..ProviderProtocolContract::default()
    };

    let result = validate_model_request_with_capabilities(&request, Some(&capabilities));

    assert_eq!(
        result.errors,
        vec![
            "provider_does_not_support_developer_messages",
            "provider_does_not_support_json_mode",
            "provider_does_not_support_strict_tool_schema",
            "provider_does_not_support_tools",
        ]
    );
}

#[test]
fn openai_provider_debug_redacts_secret_configuration() {
    let config = provider_test_config("https://provider.example/v1".to_string());
    let provider = OpenAiProvider::new(config.clone()).expect("provider");
    let config_debug = format!("{config:?}");
    let provider_debug = format!("{provider:?}");

    for debug_text in [config_debug, provider_debug] {
        assert!(!debug_text.contains("sk-secret-value"));
        assert!(!debug_text.contains("provider.example"));
        assert!(debug_text.contains("[redacted]"));
    }
}

#[test]
fn openai_provider_roundtrips_non_stream_response_without_raw_body_leak() {
    let body = r#"{
        "id": "resp_1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "done",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response");
    let serialized = serde_json::to_string(&response).expect("serialize response");

    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.response_id, "resp_1");
    assert_eq!(response.usage.total_tokens, 5);
    let metadata = response
        .provider_attempt_metadata
        .as_ref()
        .expect("non-stream success attempt metadata");
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one non-stream success occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Ok);
    assert!(occurrence.time_to_first_text_delta_ms.is_none());
    assert_eq!(
        occurrence.usage.as_ref().map(|usage| usage.total_tokens),
        Some(5)
    );
    assert_eq!(
        response.tool_calls[0].arguments,
        serde_json::json!({"path": "README.md"})
    );
    assert!(!serialized.contains("sk-secret-value"));
    assert!(!serialized.contains("choices"));
}

#[test]
fn openai_provider_retries_transient_http_errors_with_attempt_metadata() {
    let success_body = r#"{
        "id": "resp_retry",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server(vec![
        ("HTTP/1.1 429 Too Many Requests", "{}"),
        ("HTTP/1.1 503 Service Unavailable", "{}"),
        ("HTTP/1.1 200 OK", success_body),
    ]);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response after bounded retries");
    let metadata = response
        .provider_attempt_metadata
        .expect("attempt metadata");

    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(metadata.attempt_count, 3);
    assert_eq!(metadata.retry_count, 2);
    assert!(metadata.latency_ms >= 100);
    assert_eq!(metadata.occurrences.len(), 3);
    for (offset, occurrence) in metadata.occurrences.iter().enumerate() {
        assert_eq!(occurrence.attempt_index, offset as u32 + 1);
        assert_eq!(
            occurrence.operation_phase,
            ProviderAttemptOperationPhase::Completion
        );
        assert_eq!(
            occurrence.actual_api_protocol,
            ProviderApiProtocol::OpenAiChatCompletions
        );
        assert!(occurrence.request_send_to_headers_ms.is_some());
        assert!(occurrence.queue_duration_ms.is_none());
        assert!(occurrence.time_to_first_text_delta_ms.is_none());
        assert!(occurrence.ended_at_unix_ms >= occurrence.started_at_unix_ms);
        assert!(
            occurrence.attempt_duration_ms
                <= occurrence.ended_at_unix_ms - occurrence.started_at_unix_ms + 1
        );
    }
    for occurrence in &metadata.occurrences[..2] {
        assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
        assert!(occurrence.retry_scheduled);
        assert!(occurrence.retry_backoff_ms.is_some());
        assert_eq!(
            occurrence.error_stage,
            Some(ProviderErrorStage::ResponseStatus)
        );
        assert_eq!(
            occurrence.diagnostic_code.as_deref(),
            Some("provider_http_status")
        );
        assert!(occurrence.usage.is_none());
    }
    let success = &metadata.occurrences[2];
    assert_eq!(success.terminal_status, ProviderAttemptStatus::Ok);
    assert!(!success.retry_scheduled);
    assert!(success.error_category.is_none());
    assert!(success.usage.is_none());
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn openai_provider_observes_each_retry_as_one_ordered_start_end_pair() {
    let success_body = r#"{
        "id": "resp_observed_retry",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server(vec![
        ("HTTP/1.1 429 Too Many Requests", "{}"),
        ("HTTP/1.1 503 Service Unavailable", "{}"),
        ("HTTP/1.1 200 OK", success_body),
    ]);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_observed_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();

    let response = Provider::complete_observed(
        &provider,
        &request,
        &singularity_core::CancellationToken::new(),
        &mut |event| {
            events.push(event);
            true
        },
    )
    .expect("provider response after observed retries");

    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(events.len(), 6);
    for (offset, pair) in events.chunks_exact(2).enumerate() {
        let ProviderAttemptEvent::Started(started) = &pair[0] else {
            panic!("attempt must start before its terminal event");
        };
        let ProviderAttemptEvent::Finished(finished) = &pair[1] else {
            panic!("attempt must finish before the next attempt starts");
        };
        assert_eq!(started.attempt_index, offset as u32 + 1);
        assert_eq!(started.operation_phase, finished.operation_phase);
        assert_eq!(started.provider_name, finished.provider_name);
        assert_eq!(started.model_name, finished.model_name);
        assert_eq!(started.actual_api_protocol, finished.actual_api_protocol);
        assert_eq!(started.attempt_index, finished.attempt_index);
        assert_eq!(started.started_at_unix_ms, finished.started_at_unix_ms);
    }
}

#[test]
fn openai_provider_rejects_observer_start_before_network_side_effect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind observer rejection provider");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking observer rejection provider");
    let address = listener.local_addr().expect("observer rejection address");
    let provider =
        OpenAiProvider::new(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_observer_rejected",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut event_count = 0;

    let error = Provider::complete_observed(
        &provider,
        &request,
        &singularity_core::CancellationToken::new(),
        &mut |_event| {
            event_count += 1;
            false
        },
    )
    .expect_err("observer rejection must fail closed");

    assert_eq!(event_count, 1);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_attempt_observer_failed")
    );
    let accept_error = listener
        .accept()
        .expect_err("observer rejection must prevent the HTTP connection");
    assert_eq!(accept_error.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn provider_capability_cache_lookup_observations_are_closed_and_runtime_only() {
    let (base_url, _requests, _started) = delayed_probe_server(
        vec![("HTTP/1.1 200 OK", PROBE_STRICT_PARALLEL_RESPONSE)],
        Duration::ZERO,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let cancellation = singularity_core::CancellationToken::new();

    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("initial capability negotiation");
    let cached = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &cancellation,
    )
    .expect("cached capability negotiation");

    assert_eq!(
        negotiation
            .metadata
            .cache_observations
            .iter()
            .map(|observation| observation.outcome)
            .collect::<Vec<_>>(),
        vec![ProviderCapabilityCacheLookupResult::Miss]
    );
    assert_eq!(
        cached
            .metadata
            .cache_observations
            .iter()
            .map(|observation| observation.outcome)
            .collect::<Vec<_>>(),
        vec![ProviderCapabilityCacheLookupResult::Hit]
    );

    let mut response = ModelTurnResponse::completed("runtime_response", "response", "done");
    response.provider_capability_metadata = Some(ProviderCapabilityMetadata {
        cache_observations: cached.metadata.cache_observations.clone(),
        ..negotiation.metadata
    });
    let wire = serde_json::to_value(&response).expect("serialize model response");
    assert!(wire.get("provider_capability_metadata").is_none());
    let schema = serde_json::to_value(schema_for!(ModelTurnResponse)).expect("response schema");
    assert!(
        schema["properties"]
            .get("provider_capability_metadata")
            .is_none()
    );
    assert!(
        schema["definitions"]
            .get("ProviderCapabilityCacheObservation")
            .is_none()
    );
}

#[test]
fn openai_provider_uses_external_runtime_handle_for_http_body_and_backoff() {
    let success_body = r#"{
        "id": "resp_external_runtime",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server(vec![
        ("HTTP/1.1 429 Too Many Requests", "{}"),
        ("HTTP/1.1 200 OK", success_body),
    ]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("external Tokio runtime");
    let provider = OpenAiProvider::new_with_runtime_handle(
        provider_test_config(base_url),
        runtime.handle().clone(),
    )
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_external_runtime",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut request = request;
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });

    let result = thread::spawn(move || {
        provider.complete(&request, &singularity_core::CancellationToken::new())
    })
    .join()
    .expect("join external runtime provider");
    let response = result.expect("provider response");

    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .provider_attempt_metadata
            .expect("attempt metadata")
            .attempt_count,
        2
    );
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1, 2]);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

#[test]
fn openai_provider_retries_body_transport_failures_only_to_the_attempt_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind truncated provider");
    let address = listener.local_addr().expect("truncated provider address");
    let server = thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept provider retry");
            let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
            read_provider_request(&mut reader);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\nconnection: close\r\n\r\n{}",
                )
                .expect("write truncated provider response");
        }
    });
    let provider =
        OpenAiProvider::new(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_transport_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("transport failure");
    let metadata = error.provider_attempt_metadata.expect("attempt metadata");

    assert_eq!(error.error.kind, ModelErrorKind::NetworkError);
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    assert_eq!(metadata.attempt_count, 3);
    assert_eq!(metadata.retry_count, 2);
    assert!(metadata.latency_ms >= 100);
    assert_eq!(metadata.occurrences.len(), 3);
    for (index, occurrence) in metadata.occurrences.iter().enumerate() {
        assert_eq!(occurrence.attempt_index, index as u32 + 1);
        assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
        assert_eq!(
            occurrence.error_stage,
            Some(ProviderErrorStage::ResponseBodyRead)
        );
        assert_eq!(
            occurrence.diagnostic_code.as_deref(),
            Some("provider_response_body_read_failed")
        );
        assert_eq!(occurrence.retry_scheduled, index < 2);
        assert!(occurrence.time_to_first_text_delta_ms.is_none());
    }
    server.join().expect("join truncated provider");
}

#[test]
fn openai_provider_records_each_send_failure_without_headers_or_sensitive_request_data() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused provider address");
    let address = listener.local_addr().expect("unused provider address");
    drop(listener);
    let provider =
        OpenAiProvider::new(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_send_failure",
        vec![ModelMessage::text(
            ModelRole::User,
            "sensitive prompt marker",
        )],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("closed address must fail during send");
    let metadata = error
        .provider_attempt_metadata
        .expect("send failure attempt metadata");

    assert_eq!(metadata.attempt_count, 3);
    assert_eq!(metadata.retry_count, 2);
    assert_eq!(metadata.occurrences.len(), 3);
    for (index, occurrence) in metadata.occurrences.iter().enumerate() {
        assert_eq!(occurrence.attempt_index, index as u32 + 1);
        assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
        assert_eq!(occurrence.error_category, Some(ModelErrorCategory::Network));
        assert_eq!(
            occurrence.error_stage,
            Some(ProviderErrorStage::RequestSend)
        );
        assert_eq!(
            occurrence.diagnostic_code.as_deref(),
            Some("provider_request_send_failed")
        );
        assert!(occurrence.request_send_to_headers_ms.is_none());
        assert!(occurrence.time_to_first_text_delta_ms.is_none());
        assert!(occurrence.queue_duration_ms.is_none());
        assert!(occurrence.usage.is_none());
        assert_eq!(occurrence.retry_scheduled, index < 2);
    }
    let serialized = serde_json::to_string(&metadata).expect("serialize aggregate metadata");
    assert!(!serialized.contains("occurrences"));
    assert!(!serialized.contains("sensitive prompt marker"));
    assert!(!serialized.contains("sk-secret-value"));
}

#[test]
fn openai_provider_cancels_during_retry_backoff() {
    let (base_url, attempts) =
        sequence_response_server(vec![("HTTP/1.1 429 Too Many Requests", "{}")]);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_retry_cancel",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let cancellation = singularity_core::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (backoff_ready_tx, backoff_ready_rx) = mpsc::channel();
    let (backoff_release_tx, backoff_release_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut backoff_release_rx = Some(backoff_release_rx);
        let mut on_attempt = |event| {
            if let ProviderAttemptEvent::Finished(occurrence) = event
                && occurrence.retry_scheduled
                && let Some(release_rx) = backoff_release_rx.take()
            {
                backoff_ready_tx
                    .send(())
                    .expect("send retry backoff signal");
                release_rx.recv().expect("release retry backoff handshake");
            }
            true
        };
        result_tx
            .send(Provider::complete_observed(
                &provider,
                &request,
                &worker_cancellation,
                &mut on_attempt,
            ))
            .expect("send provider result");
    });

    assert_eq!(attempts.recv_timeout(Duration::from_secs(1)), Ok(1));
    backoff_ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("provider retry backoff was scheduled");
    cancellation.cancel();
    backoff_release_tx
        .send(())
        .expect("release retry backoff handshake");
    let error = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("provider cancellation during backoff was bounded")
        .expect_err("provider request cancelled");
    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
    let metadata = error
        .provider_attempt_metadata
        .expect("cancelled backoff attempt metadata");
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one failed attempt before cancelled backoff expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
    assert!(occurrence.request_send_to_headers_ms.is_some());
    assert!(occurrence.retry_scheduled);
    assert_eq!(
        occurrence.error_stage,
        Some(ProviderErrorStage::ResponseStatus)
    );
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 1);
}

#[test]
fn openai_provider_rejects_multiple_choices_without_retrying_or_selecting_one() {
    let body = r#"{
        "id": "resp_multiple_choices",
        "choices": [
            {"message": {"role": "assistant", "content": "first"}},
            {"message": {"role": "assistant", "content": "second"}}
        ]
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_multiple_choices",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("multiple choices must be rejected");
    let metadata = error.provider_attempt_metadata.expect("attempt metadata");

    assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(
        error.error.validation_errors,
        vec!["response_choices_count_invalid"]
    );
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
}

#[test]
fn openai_provider_cancels_an_inflight_http_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
    let address = listener.local_addr().expect("provider address");
    let (accepted_tx, accepted_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("read request line");
        assert!(first_line.contains("/v1/chat/completions"));
        accepted_tx.send(()).expect("signal accepted request");
        thread::sleep(Duration::from_secs(2));
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
        );
    });
    let provider =
        OpenAiProvider::new(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_cancel",
        vec![ModelMessage::text(ModelRole::User, "wait")],
    );
    let cancellation = singularity_core::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        result_tx
            .send(provider.complete(&request, &worker_cancellation))
            .expect("send provider result");
    });

    accepted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("provider request started");
    cancellation.cancel();
    let error = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("provider cancellation was bounded")
        .expect_err("provider request cancelled");

    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
    let metadata = error
        .provider_attempt_metadata
        .expect("cancelled send attempt metadata");
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one cancelled send occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Cancelled);
    assert!(occurrence.request_send_to_headers_ms.is_none());
    assert_eq!(
        occurrence.error_category,
        Some(ModelErrorCategory::Cancelled)
    );
    assert_eq!(occurrence.error_stage, Some(ProviderErrorStage::Cancelled));
}

#[test]
fn openai_provider_sends_assistant_tool_call_history_before_tool_result() {
    let body = r#"{
        "id": "resp_1",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }"#;
    let (base_url, captured_request) = captured_request_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut tool_message = ModelMessage::text(
        ModelRole::Tool,
        serde_json::json!({
            "ok": false,
            "content": {
                "validation_code": "command_not_string",
                "retry_inputs": [{"command": "cargo test"}],
            }
        })
        .to_string(),
    );
    tool_message.tool_call_id = Some("call_1".to_string());
    let request = ModelTurnRequest::new(
        "request_1",
        vec![
            ModelMessage::text(ModelRole::User, "hello"),
            ModelMessage::assistant_tool_calls(vec![tool_call("call_1", "read")]),
            tool_message,
        ],
    );

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response");
    let captured: serde_json::Value = serde_json::from_str(
        &captured_request
            .recv()
            .expect("captured provider request body"),
    )
    .expect("parse captured provider request body");

    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(captured["messages"][1]["role"], "assistant");
    assert!(captured["messages"][1]["content"].is_null());
    assert_eq!(
        captured["messages"][1]["tool_calls"][0]["function"]["name"],
        "read"
    );
    assert_eq!(
        captured["messages"][1]["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"README.md"}"#
    );
    assert_eq!(captured["messages"][2]["role"], "tool");
    assert_eq!(captured["messages"][2]["tool_call_id"], "call_1");
    let tool_content: serde_json::Value = serde_json::from_str(
        captured["messages"][2]["content"]
            .as_str()
            .expect("tool content string"),
    )
    .expect("structured tool content");
    assert_eq!(
        tool_content["content"]["validation_code"],
        "command_not_string"
    );
    assert_eq!(
        tool_content["content"]["retry_inputs"][0]["command"],
        "cargo test"
    );
}

#[test]
fn openai_provider_preserves_portable_tool_names_without_aliases() {
    let body = r#"{
        "id": "resp_1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;
    let (base_url, captured_request) = captured_request_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    request.tool_choice.max_tool_calls = 2;
    request.tool_choice.strict_tool_schema = true;

    let mut invalid_request = request.clone();
    invalid_request.tools[0].name = "builtin.read".to_string();
    assert_eq!(
        validate_model_request(&invalid_request).errors,
        vec!["tool_name_not_provider_portable"]
    );

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response");
    let captured: serde_json::Value = serde_json::from_str(
        &captured_request
            .recv()
            .expect("captured provider request body"),
    )
    .expect("parse captured provider request body");

    assert_eq!(captured["tools"][0]["function"]["name"], "read");
    assert_eq!(captured["tool_choice"], "auto");
    assert_eq!(captured["parallel_tool_calls"], true);
    assert_eq!(captured["tools"][0]["function"]["strict"], true);
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(response.status, ModelTurnStatus::Success);
}

#[test]
fn openai_provider_rejects_calls_above_the_agent_request_limit() {
    let body = r#"{
        "id": "resp_1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                    },
                    {
                        "id": "call_2",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"Cargo.toml\"}"}
                    }
                ]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "read a file")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });
    request.tool_choice.max_tool_calls = 1;

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response envelope");

    assert_eq!(response.status, ModelTurnStatus::Invalid);
    assert_eq!(response.tool_calls.len(), 2);
    let metadata = response
        .provider_attempt_metadata
        .as_ref()
        .expect("contract violation attempt metadata");
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    let [occurrence] = metadata.occurrences.as_slice() else {
        panic!("one invalid response attempt occurrence expected");
    };
    assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
    assert_eq!(
        occurrence.error_category,
        Some(ModelErrorCategory::JsonSchema)
    );
    assert_eq!(
        occurrence.error_stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert_eq!(
        occurrence.diagnostic_code.as_deref(),
        Some("provider_response_invalid")
    );
    assert!(occurrence.usage.is_none());
    let error = response.error.expect("contract violation error");
    assert_eq!(error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.message,
        "provider_response_invalid: max_tool_calls_exceeded"
    );
    assert_eq!(error.code.as_deref(), Some("provider_response_invalid"));
    assert_eq!(error.stage, Some(ProviderErrorStage::ResponseValidation));
    assert_eq!(error.validation_errors, vec!["max_tool_calls_exceeded"]);
}

#[test]
fn openai_provider_classifies_http_auth_errors_without_body_or_secret_leak() {
    let base_url = single_response_server(
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":{"message":"bad key sk-secret-value"}}"#,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("auth error");
    let serialized = serde_json::to_string(&error.error).expect("serialize error");

    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
    assert!(error.error.message.contains("HTTP 401"));
    let metadata = error
        .provider_attempt_metadata
        .as_ref()
        .expect("auth attempt metadata");
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    assert_eq!(metadata.occurrences.len(), 1);
    assert!(!serialized.contains("bad key"));
    assert!(!serialized.contains("sk-secret-value"));
    let serialized_metadata = serde_json::to_string(metadata).expect("serialize attempt aggregate");
    assert!(!serialized_metadata.contains("occurrences"));
    assert!(!serialized_metadata.contains("openai_compatible"));
    assert!(!serialized_metadata.contains("gpt-test"));
}

#[test]
fn openai_provider_classifies_model_rate_limit_and_overload_http_errors() {
    for (status_line, expected_kind, expected_category) in [
        (
            "HTTP/1.1 404 Not Found",
            ModelErrorKind::InvalidRequest,
            ModelErrorCategory::InvalidRequest,
        ),
        (
            "HTTP/1.1 429 Too Many Requests",
            ModelErrorKind::RateLimited,
            ModelErrorCategory::ProviderUnavailable,
        ),
        (
            "HTTP/1.1 500 Internal Server Error",
            ModelErrorKind::ProviderOverloaded,
            ModelErrorCategory::ProviderUnavailable,
        ),
    ] {
        let body = r#"{"error":{"message":"provider body must not leak"}}"#;
        let responses =
            if status_line.starts_with("HTTP/1.1 4") && !status_line.starts_with("HTTP/1.1 429") {
                vec![(status_line, body)]
            } else {
                vec![(status_line, body); 3]
            };
        let (base_url, attempts) = sequence_response_server(responses);
        let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
        let request = ModelTurnRequest::new(
            "request_1",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let error = provider
            .complete(&request, &singularity_core::CancellationToken::new())
            .expect_err("http error");
        let serialized = serde_json::to_string(&error.error).expect("serialize error");

        assert_eq!(error.error.kind, expected_kind);
        assert_eq!(error.error.category(), expected_category);
        if status_line.starts_with("HTTP/1.1 404") {
            assert_eq!(error.error.message, "Provider returned HTTP 404.");
        }
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("http attempt metadata");
        if status_line.starts_with("HTTP/1.1 429") || status_line.starts_with("HTTP/1.1 5") {
            assert_eq!(metadata.attempt_count, 3);
            assert_eq!(metadata.retry_count, 2);
        } else {
            assert_eq!(metadata.attempt_count, 1);
            assert_eq!(metadata.retry_count, 0);
        }
        assert!(!serialized.contains("provider body must not leak"));
        assert!(attempts.try_iter().count() >= 1);
    }
}

#[test]
fn openai_provider_validation_rejects_non_object_tool_arguments() {
    let body = r#"{
        "id": "resp_1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "\"README.md\""}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    });

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response");

    assert_eq!(response.status, ModelTurnStatus::Invalid);
    assert_eq!(
        response.validation.as_ref().expect("validation").errors,
        vec!["schema_mismatch", "tool_call_arguments_must_be_object"]
    );
    assert_eq!(
        response.error.as_ref().expect("validation error").kind,
        ModelErrorKind::JsonSchemaViolation
    );
}

#[test]
fn provider_status_reports_required_env_missing_blocker() {
    let status = ProviderConfigurationStatus::from_config(&ModelProviderConfig {
        provider_name: Some("openai_compatible".to_string()),
        model_name: None,
        base_url_present: true,
        api_key_present: false,
    });

    assert!(!status.configured);
    assert_eq!(status.blocker, Some(ModelBlockerKind::RequiredEnvMissing));
    assert_eq!(
        status.blocker.as_ref().unwrap().as_str(),
        "required env missing"
    );
}

#[test]
fn model_errors_classify_provider_failures_by_typed_cause_without_transport_calls() {
    let auth = ModelError::new(ModelErrorKind::AuthError, "Provider returned HTTP 401.")
        .with_provider("openai_compatible")
        .with_model("gpt-test");

    assert_eq!(
        classify_model_error(&auth),
        ModelErrorCategory::Authentication
    );
    let permission_denied = ModelError::new(
        ModelErrorKind::NetworkError,
        "[WinError 10013] socket access denied",
    );

    assert_eq!(permission_denied.category(), ModelErrorCategory::Network);

    let model_missing = ModelError::new(
        ModelErrorKind::InvalidRequest,
        "model gpt-missing does not exist",
    );

    assert_eq!(model_missing.category(), ModelErrorCategory::InvalidRequest);

    for code in [
        "provider_configuration_missing",
        "provider_configuration_invalid",
    ] {
        let typed_configuration = ModelError::new(
            ModelErrorKind::InvalidRequest,
            "an unrelated provider configuration message",
        )
        .with_provider_diagnostic(code, ProviderErrorStage::ClientInitialization);

        assert_eq!(
            typed_configuration.category(),
            ModelErrorCategory::ModelConfiguration
        );
    }
}

#[test]
fn request_and_response_validation_helpers_reject_empty_or_mismatched_envelopes() {
    let mut request = ModelTurnRequest::new("request_1", vec![]);
    request.model_preferences.model_name = Some("gpt-test".to_string());

    let request_result = validate_model_request(&request);
    assert!(!request_result.valid);
    assert_eq!(request_result.errors, vec!["messages_required"]);

    let response = ModelTurnResponse::completed("other_request", "response_1", "done");
    let response_result =
        validate_model_turn_response(&request, &response, &["read_file".to_string()], None);

    assert!(!response_result.valid);
    assert_eq!(response_result.errors, vec!["response_request_id_mismatch"]);
}

#[test]
fn model_turn_response_validation_rejects_text_tool_envelope_after_tool_history() {
    let mut request = ModelTurnRequest::new(
        "request_finalization",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request
        .messages
        .push(ModelMessage::assistant_tool_calls(vec![tool_call(
            "call_read",
            "read",
        )]));
    let mut tool_result = ModelMessage::text(ModelRole::Tool, r#"{"ok":true}"#);
    tool_result.tool_call_id = Some("call_read".to_string());
    request.messages.push(tool_result);
    request.tool_choice = ToolChoicePolicy {
        mode: ToolChoiceMode::None,
        max_tool_calls: 0,
        strict_tool_schema: false,
    };

    let envelope = "<tool_call><function=read></function></tool_call>";
    let response = ModelTurnResponse::completed(
        request.request_id.clone(),
        "response_finalization",
        envelope,
    );
    let result = validate_model_turn_response(&request, &response, &[], None);

    assert!(!result.valid);
    assert_eq!(result.errors, vec!["text_tool_call_envelope_not_supported"]);

    let direct_request = ModelTurnRequest::new(
        "request_direct",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let direct_response = ModelTurnResponse::completed(
        direct_request.request_id.clone(),
        "response_direct",
        envelope,
    );
    let direct_result = validate_model_turn_response(&direct_request, &direct_response, &[], None);

    assert!(direct_result.valid);
}

#[test]
fn model_error_serializes_redacted_boundary_fields() {
    let mut failure = ModelError::new(ModelErrorKind::Timeout, "provider transport failed")
        .with_provider("openai_compatible")
        .with_model("gpt-test");
    failure.timeout_seconds = Some(120);

    let value = serde_json::to_value(&failure).expect("serialize provider failure");

    assert_eq!(value["kind"], "timeout");
    assert_eq!(value["timeout_seconds"], 120);
    assert_eq!(value["provider_name"], "openai_compatible");
    assert_eq!(value["model_name"], "gpt-test");
    assert!(!value.to_string().contains("sk-"));
}

#[test]
fn model_response_validation_enforces_tool_choice_and_provider_capabilities() {
    let call = tool_call("call_1", "read_file");
    let none_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        std::slice::from_ref(&call),
        &ToolChoicePolicy {
            mode: ToolChoiceMode::None,
            ..Default::default()
        },
        &["read_file".to_string()],
        None,
    );

    assert!(!none_result.valid);
    assert_eq!(none_result.errors, vec!["tool_choice_none"]);

    let text_tool_call = validate_model_response(
        Some(&ModelMessage::text(
            ModelRole::Assistant,
            "<tool_call>\n<function=read>\n<parameter=path>Cargo.toml</parameter>\n</function>\n</tool_call>",
        )),
        &[],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        Some(&ProviderProtocolContract::default()),
    );

    assert_eq!(
        text_tool_call.errors,
        vec!["text_tool_call_envelope_not_supported"]
    );

    let prefixed_multiple_text_tool_calls = validate_model_response(
        Some(&ModelMessage::text(
            ModelRole::Assistant,
            "I will inspect both files.<tool_call><function=read></function></tool_call><tool_call><function=read></function></tool_call>",
        )),
        &[],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        Some(&ProviderProtocolContract::default()),
    );
    assert_eq!(
        prefixed_multiple_text_tool_calls.errors,
        vec!["text_tool_call_envelope_not_supported"]
    );

    let incomplete_marker = validate_model_response(
        Some(&ModelMessage::text(
            ModelRole::Assistant,
            "The literal <tool_call> marker is unsupported.",
        )),
        &[],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        Some(&ProviderProtocolContract::default()),
    );
    assert!(incomplete_marker.valid);

    let duplicate_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call.clone(), call],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        Some(&ProviderProtocolContract::default()),
    );

    assert_eq!(
        duplicate_result.errors,
        vec![
            "duplicate_tool_call_id",
            "max_tool_calls_exceeded",
            "provider_does_not_support_parallel_tool_calls"
        ]
    );
}

#[test]
fn model_response_validation_reports_unknown_tools_without_hiding_structural_errors() {
    let mut malformed = tool_call("call_1", "unknown");
    malformed.parse_status = ModelToolParseStatus::InvalidJson;
    malformed
        .validation_errors
        .push("schema detail".to_string());

    let result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[malformed],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        None,
    );

    assert!(!result.valid);
    assert_eq!(result.errors, vec!["invalid_json"]);
    assert_eq!(result.warnings, vec!["unknown_tool", "schema detail"]);
}

#[test]
fn model_response_validation_rejects_nonportable_tool_names() {
    let call = tool_call("call_1", "builtin.read");
    let assistant = ModelMessage::assistant_tool_calls(vec![call.clone()]);

    let result = validate_model_response(
        Some(&assistant),
        &[call],
        &ToolChoicePolicy::default(),
        &["read".to_string()],
        None,
    );

    assert_eq!(result.errors, vec!["tool_name_not_provider_portable"]);
}

#[test]
fn model_response_validation_requires_tool_call_arguments_object() {
    let mut call = tool_call("call_1", "read_file");
    call.arguments = serde_json::json!("not an object");

    let result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call],
        &ToolChoicePolicy::default(),
        &["read_file".to_string()],
        None,
    );

    assert!(!result.valid);
    assert_eq!(result.errors, vec!["tool_call_arguments_must_be_object"]);
}

#[test]
fn model_boundary_objects_are_schema_backed_and_round_trip() {
    let tool_schema = ModelToolSchema {
        name: "read_file".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    };
    let provider_config = ModelProviderConfig {
        provider_name: Some("openai_compatible".to_string()),
        model_name: Some("gpt-test".to_string()),
        base_url_present: true,
        api_key_present: true,
    };
    let model_error = ModelError::new(ModelErrorKind::Timeout, "provider timed out")
        .with_provider("openai_compatible")
        .with_model("gpt-test");
    let attempt_metadata = ProviderAttemptMetadata {
        attempt_count: 3,
        retry_count: 2,
        latency_ms: 150,
        occurrences: Vec::new(),
    };
    let mut runtime_attempt_metadata = attempt_metadata.clone();
    runtime_attempt_metadata
        .occurrences
        .push(ProviderAttemptOccurrence {
            operation_phase: ProviderAttemptOperationPhase::Completion,
            provider_name: "runtime-provider-marker".to_string(),
            model_name: "runtime-model-marker".to_string(),
            actual_api_protocol: ProviderApiProtocol::OpenAiResponses,
            attempt_index: 1,
            terminal_status: ProviderAttemptStatus::Ok,
            started_at_unix_ms: 1,
            ended_at_unix_ms: 2,
            attempt_duration_ms: 25,
            request_send_to_headers_ms: Some(10),
            queue_duration_ms: None,
            time_to_first_text_delta_ms: Some(15),
            retry_scheduled: false,
            retry_backoff_ms: None,
            error_category: None,
            error_stage: None,
            diagnostic_code: None,
            usage: Some(ModelUsage::default()),
            model_turn_ordinal: None,
            parent_occurrence_id: None,
        });
    let runtime_wire = serde_json::to_value(&runtime_attempt_metadata)
        .expect("serialize runtime attempt metadata");
    assert_eq!(
        runtime_wire,
        serde_json::json!({
            "attempt_count": 3,
            "retry_count": 2,
            "latency_ms": 150
        })
    );
    let restored_runtime: ProviderAttemptMetadata =
        serde_json::from_value(runtime_wire).expect("deserialize aggregate attempt metadata");
    assert_eq!(restored_runtime, attempt_metadata);
    let attempt_schema = schema_for!(ProviderAttemptMetadata);
    assert!(
        !attempt_schema
            .schema
            .object
            .as_ref()
            .expect("attempt metadata object schema")
            .properties
            .contains_key("occurrences")
    );
    assert!(
        !attempt_schema
            .definitions
            .contains_key("ProviderAttemptOccurrence")
    );
    let restored_schema: ModelToolSchema =
        serde_json::from_value(serde_json::to_value(&tool_schema).unwrap()).unwrap();
    let restored_config: ModelProviderConfig =
        serde_json::from_value(serde_json::to_value(&provider_config).unwrap()).unwrap();
    let restored_error: ModelError =
        serde_json::from_value(serde_json::to_value(&model_error).unwrap()).unwrap();
    let restored_attempt_metadata: ProviderAttemptMetadata =
        serde_json::from_value(serde_json::to_value(&attempt_metadata).unwrap()).unwrap();

    assert_eq!(restored_schema, tool_schema);
    assert_eq!(restored_config, provider_config);
    assert_eq!(restored_error, model_error);
    assert_eq!(restored_attempt_metadata, attempt_metadata);
    assert_eq!(schema_title::<ModelToolSchema>(), "ModelToolSchema");
    assert_eq!(schema_title::<ModelToolCall>(), "ModelToolCall");
    assert_eq!(
        schema_title::<ProviderProtocolContract>(),
        "ProviderProtocolContract"
    );
    assert_eq!(schema_title::<ModelProviderConfig>(), "ModelProviderConfig");
    assert_eq!(schema_title::<ModelUsage>(), "ModelUsage");
    assert_eq!(
        schema_title::<ProviderAttemptMetadata>(),
        "ProviderAttemptMetadata"
    );
    assert_eq!(schema_title::<ModelTurnRequest>(), "ModelTurnRequest");
    assert_eq!(schema_title::<ModelTurnResponse>(), "ModelTurnResponse");
}

fn schema_title<T: schemars::JsonSchema>() -> String {
    schema_for!(T)
        .schema
        .metadata
        .expect("schema metadata")
        .title
        .expect("schema title")
}
