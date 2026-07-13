use schemars::schema_for;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelBlockerKind, ModelError,
    ModelErrorCategory, ModelErrorKind, ModelMessage, ModelPreferences, ModelProviderConfig,
    ModelRole, ModelToolCall, ModelToolParseStatus, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, OpenAiProvider, OpenAiProviderConfig, Provider,
    ProviderAttemptMetadata, ProviderCapabilityProfile, ProviderConfigSnapshot,
    ProviderConfigSource, ProviderConfigurationStatus, ProviderErrorStage,
    ProviderProtocolContract, ToolChoiceMode, ToolChoicePolicy, chat_completions_endpoint,
    classify_model_error, resolve_provider_config, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Barrier, Mutex,
    mpsc::{self, Receiver},
};
use std::thread;
use std::time::Duration;

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
        name: "builtin.read".to_string(),
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

fn strict_probe_server() -> (String, Receiver<String>) {
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
        tx.send(request_body.clone())
            .expect("send strict capability request");

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
            ("HTTP/1.1 200 OK", PROBE_STRICT_PARALLEL_RESPONSE),
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
        }
        request_tx
            .send(seen_requests)
            .expect("send delayed capability requests");
    });
    (format!("http://{addr}"), request_rx, started_rx)
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

const PROBE_ROUTER_RESPONSE: &str = r#"{
    "id": "probe_router",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "probe_router_call",
                "type": "function",
                "function": {
                    "name": "singularity_capability_router",
                    "arguments": "{\"tool_name\":\"singularity_capability_probe_a\",\"arguments\":{\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}}"
                }
            }]
        },
        "finish_reason": "tool_calls"
    }],
    "usage": {"prompt_tokens": 2, "completion_tokens": 1, "total_tokens": 3}
}"#;

const ACTUAL_DONE_RESPONSE: &str = r#"{
    "id": "actual_response",
    "choices": [{
        "message": {"role": "assistant", "content": "done"},
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
    if names.contains(&"singularity_capability_router") {
        return Some(PROBE_ROUTER_RESPONSE.to_string());
    }
    if !names.contains(&"singularity_capability_probe_a") {
        return None;
    }
    let strict = tools.iter().any(|tool| {
        tool.pointer("/function/strict")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    });
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
    assert_eq!(
        default_config.protocol_contract().max_tool_calls_per_turn,
        1
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
        "SINGULARITY_MODEL_CONTEXT_TOKENS" => Some(" 131072 ".to_string()),
        "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS" => Some("8192".to_string()),
        _ => None,
    })
    .expect("configured provider");
    let capabilities = configured.protocol_contract();
    assert_eq!(capabilities.max_context_tokens, 131_072);
    assert_eq!(capabilities.max_output_tokens, 8_192);
    assert_eq!(capabilities.max_tool_calls_per_turn, 1);
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
    assert_eq!(negotiation.contract.max_tool_calls_per_turn, 2);
    assert_eq!(negotiation.contract.max_tools_per_request, 8);

    let request: serde_json::Value = serde_json::from_str(
        &request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("captured strict capability request"),
    )
    .expect("strict capability request JSON");
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
        "Call singularity_capability_probe_a and singularity_capability_probe_b exactly once each."
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
    assert_eq!(negotiation.contract.max_tool_calls_per_turn, 2);
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
    assert_eq!(tool_counts, vec![8]);
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
        assert_eq!(negotiation.contract.max_tool_calls_per_turn, 2);
        assert_eq!(negotiation.contract.max_tools_per_request, 8);
        assert_eq!(negotiation.metadata.profile_attempts, 3);
        assert_eq!(negotiation.metadata.fallback_count, 2);
        done.recv_timeout(Duration::from_secs(1))
            .expect("strict constraint probe fallback completed");
    }
}

#[test]
fn openai_capability_probe_non_strict_single_uses_required_tool_choice() {
    let (base_url, requests, _started) = delayed_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_STRICT_SINGLE_RESPONSE),
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
    assert_eq!(single_request["tool_choice"], "required");
    assert_eq!(single_request["tools"].as_array().map(Vec::len), Some(8));
    assert_eq!(single_request["tools"][0]["function"].get("strict"), None);
}

#[test]
fn openai_capability_probe_validates_router_before_low_capacity_fallback() {
    let (base_url, requests, _started) = delayed_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_ROUTER_RESPONSE),
        ],
        Duration::ZERO,
    );
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");

    let negotiation = Provider::negotiate_tool_capabilities(
        &provider,
        &ModelPreferences::default(),
        &singularity_core::CancellationToken::new(),
    )
    .expect("validated router fallback");

    assert_eq!(
        negotiation.metadata.profile,
        ProviderCapabilityProfile::NonStrictSingle
    );
    assert_eq!(negotiation.contract.max_tool_calls_per_turn, 1);
    assert_eq!(negotiation.contract.max_tools_per_request, 1);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured capability requests");
    assert_eq!(
        captured
            .iter()
            .map(|request| {
                serde_json::from_str::<serde_json::Value>(request).expect("probe JSON")["tools"]
                    .as_array()
                    .map(Vec::len)
                    .expect("probe tools")
            })
            .collect::<Vec<_>>(),
        vec![8, 8, 8, 8, 1]
    );
    let router: serde_json::Value =
        serde_json::from_str(captured.last().expect("router request")).expect("router JSON");
    assert_eq!(
        router["tools"][0]["function"]["name"],
        "singularity_capability_router"
    );
    let branches = router["tools"][0]["function"]["parameters"]["oneOf"]
        .as_array()
        .expect("router branches");
    assert_eq!(branches.len(), 8);
    assert_eq!(
        branches[0]["required"],
        serde_json::json!(["tool_name", "arguments"])
    );
    assert_eq!(
        branches[0]["properties"]["tool_name"]["const"],
        "singularity_capability_probe_a"
    );
    assert_eq!(
        branches[0]["properties"]["arguments"]["required"],
        serde_json::json!(["probe", "values"])
    );
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
    assert_eq!(negotiation.contract.max_tool_calls_per_turn, 1);
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
        5
    );

    let captured = requests
        .recv_timeout(Duration::from_secs(2))
        .expect("captured capability probes");
    assert_eq!(captured.len(), 5, "one shared probe round is required");
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
        1,
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
        2,
        "waiter must take over after owner cancellation"
    );
}

#[test]
fn openai_capability_probe_fallback_keeps_usage_attempts_and_cache_metadata_separate() {
    let (base_url, requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 400 Bad Request", "{}"),
            ("HTTP/1.1 200 OK", PROBE_ROUTER_RESPONSE),
        ],
        ACTUAL_DONE_RESPONSE,
        1,
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
    assert_eq!(negotiation.contract.max_tool_calls_per_turn, 1);
    assert_eq!(negotiation.contract.max_tools_per_request, 1);
    assert_eq!(negotiation.metadata.profile_attempts, 5);
    assert_eq!(negotiation.metadata.fallback_count, 4);
    assert_eq!(negotiation.metadata.probe_usage.total_tokens, 3);
    assert_eq!(negotiation.metadata.probe_attempt_metadata.attempt_count, 5);
    assert_eq!(negotiation.metadata.probe_attempt_metadata.retry_count, 0);

    provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("actual completion after cache hit");
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured actual request");
    assert_eq!(captured.len(), 1, "cache hit must avoid a second probe");
    let actual: serde_json::Value = serde_json::from_str(&captured[0]).expect("actual JSON");
    assert_eq!(actual["tools"][0]["function"].get("strict"), None);
}

#[test]
fn openai_capability_probe_final_text_response_is_typed_and_preserves_probe_metadata() {
    let (base_url, requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
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
    assert_eq!(metadata.profile_attempts, 5);
    assert_eq!(metadata.fallback_count, 4);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 5);
    assert!(requests.recv_timeout(Duration::from_secs(1)).is_ok());
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
    assert_eq!(metadata.profile_attempts, 5);
    assert_eq!(metadata.fallback_count, 4);
    assert_eq!(metadata.probe_attempt_metadata.attempt_count, 5);
    assert!(requests.recv_timeout(Duration::from_secs(1)).is_ok());
}

#[test]
fn openai_capability_probe_preserves_structured_validation_errors() {
    let (base_url, _requests) = configurable_probe_server(
        vec![
            ("HTTP/1.1 400 Bad Request", "{}"),
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
        probe_count, 2,
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

    assert_eq!(config.protocol_contract().max_tool_calls_per_turn, 1);
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
fn model_request_validation_rejects_tool_count_above_provider_capability() {
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "builtin.read".to_string(),
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
        vec!["requested_tool_calls_exceed_provider_limit"]
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
            name: format!("builtin.tool_{index}"),
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
        name: "builtin.read".to_string(),
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
        name: "builtin.read".to_string(),
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
                    "function": {"name": "builtin.read", "arguments": "{\"path\":\"README.md\"}"}
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
        name: "builtin.read".to_string(),
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
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1, 2, 3]);
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
    server.join().expect("join truncated provider");
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
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        result_tx
            .send(provider.complete(&request, &worker_cancellation))
            .expect("send provider result");
    });

    assert_eq!(attempts.recv_timeout(Duration::from_secs(1)), Ok(1));
    cancellation.cancel();
    let error = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("provider cancellation during backoff was bounded")
        .expect_err("provider request cancelled");
    let metadata = error.provider_attempt_metadata.expect("attempt metadata");

    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
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
                "validation_code": "argv_not_array",
                "retry_inputs": [{"argv": ["cargo", "test"]}],
            }
        })
        .to_string(),
    );
    tool_message.tool_call_id = Some("call_1".to_string());
    let request = ModelTurnRequest::new(
        "request_1",
        vec![
            ModelMessage::text(ModelRole::User, "hello"),
            ModelMessage::assistant_tool_calls(vec![tool_call("call_1", "builtin.read")]),
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
    assert_eq!(tool_content["content"]["validation_code"], "argv_not_array");
    assert!(tool_content["content"]["retry_inputs"][0]["argv"].is_array());
}

#[test]
fn openai_provider_projects_negotiated_tool_capabilities_and_wire_names() {
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
        name: "builtin.read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    });
    request.tool_choice.mode = ToolChoiceMode::SpecificTool;
    request.tool_choice.tool_name = Some("builtin.read".to_string());
    request.tool_choice.max_tool_calls = 2;
    request.tool_choice.strict_tool_schema = true;

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
    assert_eq!(
        captured["tool_choice"],
        serde_json::json!({"type": "function", "function": {"name": "read"}})
    );
    assert_eq!(captured["parallel_tool_calls"], true);
    assert_eq!(captured["tools"][0]["function"]["strict"], true);
    assert_eq!(response.tool_calls[0].tool_name, "builtin.read");
    assert_eq!(response.status, ModelTurnStatus::Success);
}

#[test]
fn openai_provider_classifies_calls_above_the_negotiated_limit() {
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
        name: "builtin.read".to_string(),
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
    let error = response.error.expect("contract violation error");
    assert_eq!(error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.message,
        "provider returned more tool calls than the negotiated limit of 1"
    );
    assert_eq!(
        error.code.as_deref(),
        Some("provider_tool_call_limit_exceeded")
    );
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
    assert!(!serialized.contains("bad key"));
    assert!(!serialized.contains("sk-secret-value"));
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
                    "function": {"name": "builtin.read", "arguments": "\"README.md\""}
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
        name: "builtin.read".to_string(),
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
    let response_result = validate_model_turn_response(
        &request,
        &response,
        &["builtin.read_file".to_string()],
        None,
    );

    assert!(!response_result.valid);
    assert_eq!(response_result.errors, vec!["response_request_id_mismatch"]);
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
    let call = tool_call("call_1", "builtin.read_file");
    let none_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        std::slice::from_ref(&call),
        &ToolChoicePolicy {
            mode: ToolChoiceMode::None,
            ..Default::default()
        },
        &["builtin.read_file".to_string()],
        None,
    );

    assert!(!none_result.valid);
    assert_eq!(none_result.errors, vec!["tool_choice_none"]);

    let specific_without_call = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "not a tool call")),
        &[],
        &ToolChoicePolicy {
            mode: ToolChoiceMode::SpecificTool,
            tool_name: Some("builtin.read_file".to_string()),
            ..Default::default()
        },
        &["builtin.read_file".to_string()],
        None,
    );

    assert_eq!(specific_without_call.errors, vec!["specific_tool_required"]);

    let duplicate_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call.clone(), call],
        &ToolChoicePolicy::default(),
        &["builtin.read_file".to_string()],
        Some(&ProviderProtocolContract::default()),
    );

    assert_eq!(
        duplicate_result.errors,
        vec![
            "duplicate_tool_call_id",
            "max_tool_calls_exceeded",
            "provider_tool_call_limit_exceeded"
        ]
    );
}

#[test]
fn model_response_validation_rejects_unknown_or_malformed_tool_calls() {
    let mut malformed = tool_call("call_1", "builtin.unknown");
    malformed.parse_status = ModelToolParseStatus::InvalidJson;
    malformed
        .validation_errors
        .push("schema detail".to_string());

    let result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[malformed],
        &ToolChoicePolicy::default(),
        &["builtin.read_file".to_string()],
        None,
    );

    assert!(!result.valid);
    assert_eq!(result.errors, vec!["invalid_json", "unknown_tool"]);
    assert_eq!(result.warnings, vec!["schema detail"]);
}

#[test]
fn model_response_validation_requires_tool_call_arguments_object() {
    let mut call = tool_call("call_1", "builtin.read_file");
    call.arguments = serde_json::json!("not an object");

    let result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call],
        &ToolChoicePolicy::default(),
        &["builtin.read_file".to_string()],
        None,
    );

    assert!(!result.valid);
    assert_eq!(result.errors, vec!["tool_call_arguments_must_be_object"]);
}

#[test]
fn model_boundary_objects_are_schema_backed_and_round_trip() {
    let tool_schema = ModelToolSchema {
        name: "builtin.read_file".to_string(),
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
    };
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
