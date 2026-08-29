//! model 层进程边界测试：本地假 HTTP 服务器驱动真实 reqwest 栈，只固定
//! 敏感信息不外泄与响应体体积上限这两类失败后不可见的护栏；流式解析、
//! 目录选择与校验错误文本的行为回归由评估器与真实使用兜底。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use singularity_model::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelMessage, ModelRole,
    ModelTurnRequest, ModelTurnResponse, OpenAiProvider, OpenAiProviderConfig, Provider,
    ProviderApiProtocol, ProviderConfigSource, ProviderError, ProviderErrorStage,
    chat_completions_endpoint,
};

fn tool_call_fixture() -> Vec<singularity_model::ModelToolSchema> {
    vec![singularity_model::ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: serde_json::json!({"type": "object"}),
    }]
}

fn provider_config_with_base_url(base_url: String) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "test-model".to_string(),
        base_url,
        api_key: "test-key-placeholder".to_string(),
        source: ProviderConfigSource::UserConfigFile,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

fn provider_test_config(base_url: String) -> OpenAiProviderConfig {
    provider_config_with_base_url(chat_completions_endpoint(&base_url))
}

/// 测试共享的注入 runtime：provider 的异步执行一律由上层提供。
fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("shared test provider runtime")
        })
        .handle()
        .clone()
}

/// 构造注入共享测试 runtime 的 provider；协议按 base_url 端点后缀选择
/// （`/responses` 结尾为 Responses，否则 Chat）。
fn test_provider(config: OpenAiProviderConfig) -> Result<OpenAiProvider, ProviderError> {
    let api_protocol = if config
        .base_url
        .trim()
        .trim_end_matches('/')
        .ends_with("/responses")
    {
        ProviderApiProtocol::OpenAiResponses
    } else {
        ProviderApiProtocol::OpenAiChatCompletions
    };
    OpenAiProvider::with_single_model(config, api_protocol, test_runtime_handle())
}

/// 单次请求即返回固定响应体的假 provider 服务器；返回 base_url。
fn single_response_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test provider");
    let addr = listener.local_addr().expect("test provider address");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept test provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let (first_line, headers, _) = read_provider_request(&mut reader);
        assert!(first_line.contains("/v1/chat/completions"));
        assert!(headers.contains("authorization: Bearer test-key-placeholder"));
        write_provider_response(&mut stream, status_line, body);
    });
    format!("http://{addr}")
}

/// Responses 协议的流式服务器：按 chunk 写出事件，客户端读 body。
fn responses_stream_server(chunks: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind Responses stream provider");
    let addr = listener
        .local_addr()
        .expect("Responses stream provider address");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept Responses stream request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        read_provider_request(&mut reader);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
        )
        .expect("write Responses stream headers");
        stream.flush().expect("flush Responses stream headers");
        for chunk in chunks {
            stream
                .write_all(&chunk)
                .expect("write Responses stream chunk");
            stream.flush().expect("flush Responses stream chunk");
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    format!("http://{addr}/v1/responses")
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

fn write_provider_response(stream: &mut TcpStream, status_line: &str, body: &str) {
    write!(
        stream,
        "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write provider response");
}

#[test]
fn model_turn_schema_excludes_runtime_and_trace_metadata() {
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let value = serde_json::to_value(&request).expect("serialize model request");

    assert_eq!(value["tools"], serde_json::json!([]));
    assert_eq!(value["tool_choice"]["max_tool_calls"], 1);
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
    // tool_calls 唯一存储于 assistant message；空数组按 ModelMessage 惯例省略键。
    assert!(response_value.get("tool_calls").is_none());
    assert_eq!(
        response_value["assistant_message"]["tool_calls"],
        serde_json::Value::Null
    );
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
fn provider_response_decode_and_envelope_failures_have_stable_safe_diagnostics() {
    let malformed_url = single_response_server("HTTP/1.1 200 OK", "not-json");
    let malformed = test_provider(provider_test_config(malformed_url)).expect("malformed provider");
    let request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let decode_error = malformed
        .complete(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect_err("decode failure");
    assert_eq!(
        decode_error.error.code.as_deref(),
        Some("provider_response_json_decode_failed")
    );
    assert_eq!(
        decode_error.error.stage,
        Some(ProviderErrorStage::ResponseJsonDecode)
    );

    let missing_choices_url = single_response_server("HTTP/1.1 200 OK", r#"{"id":"response_1"}"#);
    let missing_choices =
        test_provider(provider_test_config(missing_choices_url)).expect("missing choices provider");
    let envelope_error = missing_choices
        .complete(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
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
fn openai_provider_debug_redacts_secret_configuration() {
    let config = provider_test_config("https://provider.example/v1".to_string());
    let provider = test_provider(config.clone()).expect("provider");
    let config_debug = format!("{config:?}");
    let provider_debug = format!("{provider:?}");

    for debug_text in [config_debug, provider_debug] {
        assert!(!debug_text.contains("test-key-placeholder"));
        assert!(!debug_text.contains("provider.example"));
        assert!(debug_text.contains("[redacted]"));
    }
}

#[test]
fn openai_responses_stream_rejects_oversized_body_and_ignores_tool_argument_deltas() {
    let body = format!("data: {}\n\n", "x".repeat(8 * 1024 * 1024 + 1));
    let chunks = body
        .as_bytes()
        .chunks(64 * 1024)
        .map(<[u8]>::to_vec)
        .collect();
    let base_url = responses_stream_server(chunks);
    let provider = test_provider(provider_config_with_base_url(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_oversized",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let error = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
            &mut |_| {},
        )
        .expect_err("oversized stream must fail closed");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_stream_too_large")
    );

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
    let chunks = tool_body.as_bytes().chunks(5).map(<[u8]>::to_vec).collect();
    let base_url = responses_stream_server(chunks);
    let provider = test_provider(provider_config_with_base_url(base_url)).expect("provider");
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
            &mut |_| {},
        )
        .expect("final function call envelope");
    assert!(events.is_empty());
    assert_eq!(response.tool_calls().len(), 1);
    assert!(
        response.provider_reasoning_history.is_empty(),
        "Responses tool calls without reasoning must not synthesize an off replay"
    );
    assert_eq!(response.tool_calls()[0].tool_name, "read");
    assert_eq!(
        response.tool_calls()[0].raw_arguments,
        r#"{"path":"README.md"}"#
    );
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
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools = tool_call_fixture();

    let response = provider
        .complete(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect("provider response");
    let serialized = serde_json::to_string(&response).expect("serialize response");

    assert_eq!(response.status, singularity_model::ModelTurnStatus::Success);
    assert_eq!(response.response_id, "resp_1");
    assert_eq!(response.usage.total_tokens, 5);
    assert_eq!(
        response.tool_calls()[0].arguments,
        serde_json::json!({"path": "README.md"})
    );
    assert!(!serialized.contains("test-key-placeholder"));
    assert!(!serialized.contains("choices"));
}

#[test]
fn openai_provider_returns_send_failure_without_sensitive_request_data() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused provider address");
    let address = listener.local_addr().expect("unused provider address");
    drop(listener);
    let provider =
        test_provider(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_send_failure",
        vec![ModelMessage::text(
            ModelRole::User,
            "sensitive prompt marker",
        )],
    );

    let error = provider
        .complete(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect_err("closed address must fail during send");
    let serialized = serde_json::to_string(&error.error).expect("serialize error");
    assert!(!serialized.contains("sensitive prompt marker"));
    assert!(!serialized.contains("test-key-placeholder"));
}
