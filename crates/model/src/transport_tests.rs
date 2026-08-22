use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use singularity_core::CancellationToken;

use super::{
    full_jitter_delay_ms, provider_retry_backoff, retry_after_delay, retry_backoff_window_ms,
};
use crate::error::{
    ModelErrorCategory, ModelErrorKind, ProviderErrorStage, ProviderTransportCategory,
};
use crate::provider::telemetry::ProviderAttemptStatus;
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelMessage, ModelRole, ModelToolCall,
    ModelToolParseStatus, ModelTurnRequest, OpenAiProvider, OpenAiProviderConfig,
    PROVIDER_RETRY_BASE_BACKOFF_MS, PROVIDER_RETRY_MAX_BACKOFF_MS, Provider, ProviderApiProtocol,
    ProviderConfigSource, ProviderReasoningReplay, ProviderToolReasoningMode, SelectedModel,
    ThinkingWireFormat,
};

/// 测试共享的注入 runtime：provider 异步执行一律由上层提供。
fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test provider runtime")
        })
        .handle()
        .clone()
}

/// 构造注入共享测试 runtime 的 provider；返回 Result 以保持调用点 `.expect` 形状。
fn test_provider(config: OpenAiProviderConfig) -> Result<OpenAiProvider, crate::ProviderError> {
    OpenAiProvider::new(config, test_runtime_handle())
}

/// 同 [`test_provider`]，但覆盖请求超时秒数。
fn test_provider_with_timeout(
    config: OpenAiProviderConfig,
    request_timeout_seconds: u64,
) -> Result<OpenAiProvider, crate::ProviderError> {
    OpenAiProvider::new_with_request_timeout(config, request_timeout_seconds, test_runtime_handle())
}

fn tool_result_message(call_id: &str, text: &str) -> ModelMessage {
    let mut message = ModelMessage::text(ModelRole::Tool, text);
    message.tool_call_id = Some(call_id.to_string());
    message
}

fn selected_provider() -> OpenAiProvider {
    let config = OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: "test-key-placeholder".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    };
    test_provider(config)
        .expect("provider")
        .with_selected_model(SelectedModel {
            model_name: "gpt-test".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_variant: Some("on".to_string()),
            reasoning_enabled: true,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ThinkingType,
            tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
            supports_developer_role: true,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: true,
            requires_assistant_content_for_tool_calls: false,
        })
}

/// 请求侧校验：对于无推理历史的普通工具调用消息允许无绑定重放，仅拒绝重复冲突绑定。
#[test]
fn validate_reasoning_history_allows_unbound_legacy_tool_message() {
    let provider = selected_provider();
    let mut legacy = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
        tool_call_id: "legacy_call".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({"path": "x"}),
        raw_arguments: "{\"path\":\"x\"}".to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }]);
    legacy.content = "legacy".to_string();
    let mut fresh = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
        tool_call_id: "fresh_call".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({"path": "y"}),
        raw_arguments: "{\"path\":\"y\"}".to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }]);
    fresh.content = "fresh".to_string();
    let mut request = ModelTurnRequest::new(
        "validate_unbound",
        vec![
            ModelMessage::text(ModelRole::User, "hi"),
            legacy,
            tool_result_message("legacy_call", "legacy result"),
            fresh,
            tool_result_message("fresh_call", "fresh result"),
        ],
    );
    request.model_preferences.model_name = Some("provider/model#on".to_string());
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        reasoning_effort: Some("on".to_string()),
        tool_call_ids: vec!["fresh_call".to_string()],
        reasoning_content: "reasoning for fresh".to_string(),
    }];
    // legacy_call 无绑定 replay 是合法形态（v3 迁移兼容）。
    provider
        .validate_reasoning_history(&request)
        .expect("legacy tool message without replay must be accepted");
    // 重复绑定必须拒绝。
    let mut duplicated = request.clone();
    duplicated.provider_reasoning_history = vec![
        request.provider_reasoning_history[0].clone(),
        ProviderReasoningReplay::Chat {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            reasoning_effort: Some("on".to_string()),
            tool_call_ids: vec!["fresh_call".to_string()],
            reasoning_content: "another replay for fresh".to_string(),
        },
    ];
    assert!(provider.validate_reasoning_history(&duplicated).is_err());
}

#[test]
fn retry_after_parser_prefers_milliseconds_and_accepts_seconds_and_http_date() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after-ms", HeaderValue::from_static("120"));
    headers.insert("retry-after", HeaderValue::from_static("9"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(120))
    );

    headers.remove("retry-after-ms");
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_secs(9).min(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS),))
    );

    headers.insert(
        "retry-after",
        HeaderValue::from_static("Wed, 21 Oct 2030 07:28:00 GMT"),
    );
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS)),
    );
}

#[test]
fn retry_after_parser_falls_back_for_invalid_and_clamps_large_values() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("-1"));
    assert_eq!(retry_after_delay(&headers), None);

    headers.insert("retry-after", HeaderValue::from_static("999999"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS))
    );

    headers.insert("retry-after-ms", HeaderValue::from_static("not-a-duration"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS))
    );
}

#[test]
fn provider_retry_backoff_uses_full_jitter_window() {
    for retry_count in 1..=6 {
        let window = retry_backoff_window_ms(retry_count);
        assert_eq!(
            window,
            (PROVIDER_RETRY_BASE_BACKOFF_MS.saturating_mul(1_u64 << (retry_count - 1)))
                .min(PROVIDER_RETRY_MAX_BACKOFF_MS)
        );
        assert_eq!(full_jitter_delay_ms(retry_count, 0), 0);
        assert_eq!(full_jitter_delay_ms(retry_count, window), window);
        assert_eq!(full_jitter_delay_ms(retry_count, window + 1), 0);

        let delay = provider_retry_backoff(retry_count);
        assert!(
            delay <= Duration::from_millis(window),
            "retry {retry_count} produced {delay:?} outside full-jitter window"
        );
    }
    assert_eq!(retry_backoff_window_ms(8), PROVIDER_RETRY_MAX_BACKOFF_MS);
    assert_eq!(
        retry_backoff_window_ms(u32::MAX),
        PROVIDER_RETRY_MAX_BACKOFF_MS
    );
}

fn test_provider_config(base_url: String) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url,
        api_key: "test-key-placeholder".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

fn read_test_provider_request(stream: &TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read provider request line");
    assert!(line.contains("/v1/chat/completions"));
}

fn write_test_provider_response(stream: &mut TcpStream) {
    let body = r#"{"id":"response_1","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write provider response");
}

fn concurrent_provider_server() -> (String, Receiver<usize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent provider");
    let address = listener.local_addr().expect("concurrent provider address");
    let (maximum_tx, maximum_rx) = mpsc::channel();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let server = thread::spawn({
        let active = Arc::clone(&active);
        let maximum = Arc::clone(&maximum);
        move || {
            let mut workers = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept concurrent request");
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                workers.push(thread::spawn(move || {
                    read_test_provider_request(&stream);
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(150));
                    write_test_provider_response(&mut stream);
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for worker in workers {
                worker.join().expect("join concurrent provider request");
            }
            maximum_tx
                .send(maximum.load(Ordering::SeqCst))
                .expect("send concurrent provider maximum");
        }
    });
    (format!("http://{address}"), maximum_rx, server)
}

fn cancellation_followup_server() -> (String, Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancellation provider");
    let address = listener
        .local_addr()
        .expect("cancellation provider address");
    let (first_request_tx, first_request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (first_stream, _) = listener.accept().expect("accept cancelled request");
        read_test_provider_request(&first_stream);
        first_request_tx
            .send(())
            .expect("send cancelled request started");

        let (mut followup_stream, _) = listener.accept().expect("accept follow-up request");
        read_test_provider_request(&followup_stream);
        write_test_provider_response(&mut followup_stream);
        thread::sleep(Duration::from_millis(100));
    });
    (format!("http://{address}"), first_request_rx, server)
}

#[test]
fn configured_deadline_is_reported_from_a_real_transport_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
    let address = listener.local_addr().expect("provider address");
    let server = thread::spawn(move || {
        let mut streams = Vec::new();
        // 超时（挂起）不再重试：只接受 1 个连接。
        for _ in 0..1 {
            let (stream, _) = listener.accept().expect("accept provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request line");
            streams.push(stream);
        }
        thread::sleep(Duration::from_secs(2));
    });
    let provider = test_provider_with_timeout(
        OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url: format!("http://{address}"),
            api_key: "test-key-placeholder".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        },
        1,
    )
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_timeout",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &CancellationToken::new())
        .expect_err("provider request must time out");

    assert_eq!(error.error.kind, ModelErrorKind::Timeout);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_request_send_failed")
    );
    assert_eq!(error.error.stage, Some(ProviderErrorStage::RequestSend));
    assert_eq!(
        error.error.transport_category,
        Some(ProviderTransportCategory::Timeout)
    );
    assert_eq!(error.error.timeout_seconds, Some(1));
    let metadata = error
        .provider_attempt_metadata
        .as_ref()
        .expect("timeout attempt metadata");
    // 超时（挂起）不再重试：单次 120s 超时即失败，避免 6 次重试拖 12 分钟。
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    assert_eq!(metadata.occurrences.len(), 1);
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
        assert!(!occurrence.retry_scheduled);
    }
    let serialized = serde_json::to_string(&error.error).expect("serialize timeout");
    for secret in [
        "test-key-placeholder",
        &address.to_string(),
        "authorization",
    ] {
        assert!(
            !serialized
                .to_ascii_lowercase()
                .contains(&secret.to_ascii_lowercase())
        );
    }
    server.join().expect("provider server");
}

#[test]
fn oversized_success_body_is_rejected_before_buffering() {
    const OVERSIZED_RESPONSE_BYTES: usize = 8 * 1024 * 1024 + 1;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind oversized provider");
    let address = listener.local_addr().expect("provider address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {OVERSIZED_RESPONSE_BYTES}\r\nconnection: close\r\n\r\n"
        )
        .expect("write oversized response headers");
    });
    let provider = test_provider(OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url: format!("http://{address}"),
        api_key: "test-key-placeholder".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    })
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_oversized",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &CancellationToken::new())
        .expect_err("oversized provider response must fail closed");

    assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_body_too_large")
    );
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    assert_eq!(
        error.error.validation_errors,
        ["provider_response_body_too_large"]
    );
    let metadata = error
        .provider_attempt_metadata
        .as_ref()
        .expect("oversized response attempt metadata");
    assert_eq!(metadata.attempt_count, 1);
    assert_eq!(metadata.retry_count, 0);
    server.join().expect("provider server");
}

#[test]
fn provider_clones_share_a_runtime_and_requests_progress_concurrently() {
    let (base_url, maximum_rx, server) = concurrent_provider_server();
    let provider = test_provider(test_provider_config(base_url)).expect("provider");
    // 注入的 runtime 是廉价共享句柄：clone 必然绑定同一 runtime；
    // 下方的并发请求断言（maximum=2）从行为上验证两条 clone 同时执行。

    let provider = Arc::new(provider);
    let start = Arc::new(std::sync::Barrier::new(3));
    let (result_tx, result_rx) = mpsc::channel();
    let mut callers = Vec::new();
    for request_id in ["request_concurrent_a", "request_concurrent_b"] {
        let provider = Arc::clone(&provider);
        let start = Arc::clone(&start);
        let result_tx = result_tx.clone();
        callers.push(thread::spawn(move || {
            let request = ModelTurnRequest::new(
                request_id,
                vec![ModelMessage::text(ModelRole::User, "hello")],
            );
            start.wait();
            result_tx
                .send(provider.complete(&request, &CancellationToken::new()))
                .expect("send concurrent provider result");
        }));
    }
    start.wait();

    for _ in 0..2 {
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("concurrent provider request completed")
            .expect("concurrent provider request succeeded");
    }
    for caller in callers {
        caller.join().expect("join concurrent provider caller");
    }
    assert_eq!(
        maximum_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("concurrent provider maximum"),
        2,
        "shared runtime must not serialize provider requests"
    );
    server.join().expect("join concurrent provider server");
}

#[test]
fn cancelled_request_does_not_poison_followup_on_the_shared_runtime() {
    let (base_url, first_request_rx, server) = cancellation_followup_server();
    let provider = test_provider(test_provider_config(base_url)).expect("provider");
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker_provider = provider.clone();
    let worker = thread::spawn(move || {
        let request = ModelTurnRequest::new(
            "request_cancel_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "wait")],
        );
        worker_provider.complete(&request, &worker_cancellation)
    });
    first_request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancelled request reached server");
    cancellation.cancel();
    let error = worker
        .join()
        .expect("join cancel worker")
        .expect_err("cancelled request must fail");
    assert_eq!(error.error.kind, ModelErrorKind::Cancelled);

    let followup = ModelTurnRequest::new(
        "request_after_cancel_shared_runtime",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    provider
        .complete(&followup, &CancellationToken::new())
        .expect("follow-up request must succeed on shared runtime");
    server.join().expect("join cancellation provider server");
}

#[test]
fn timed_out_request_does_not_poison_followup_on_the_shared_runtime() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind timeout provider");
    let address = listener.local_addr().expect("timeout provider address");
    let server = thread::spawn(move || {
        let mut hanging_streams = Vec::new();
        // 超时（挂起）不再重试：只接受 1 个挂起连接。
        for _ in 0..1 {
            let (stream, _) = listener.accept().expect("accept timed-out request");
            read_test_provider_request(&stream);
            hanging_streams.push(stream);
        }

        let (mut followup_stream, _) = listener.accept().expect("accept timeout follow-up");
        read_test_provider_request(&followup_stream);
        write_test_provider_response(&mut followup_stream);
        drop(hanging_streams);
    });
    let provider = test_provider_with_timeout(test_provider_config(format!("http://{address}")), 1)
        .expect("provider");
    let timed_out_request = ModelTurnRequest::new(
        "request_timeout_shared_runtime",
        vec![ModelMessage::text(ModelRole::User, "wait")],
    );
    let error = provider
        .complete(&timed_out_request, &CancellationToken::new())
        .expect_err("provider request must time out");
    assert_eq!(error.error.kind, ModelErrorKind::Timeout);
    assert_eq!(
        error
            .provider_attempt_metadata
            .as_ref()
            .expect("timeout metadata")
            .attempt_count,
        // 超时（挂起）不再重试：单次超时即失败。
        1
    );

    let followup = ModelTurnRequest::new(
        "request_after_timeout_shared_runtime",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    provider
        .complete(&followup, &CancellationToken::new())
        .expect("follow-up request must still succeed");
    server.join().expect("join timeout provider server");
}

#[test]
fn streaming_response_timeout_is_idle_not_total() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow streaming provider");
    let address = listener
        .local_addr()
        .expect("slow streaming provider address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept streaming request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone streaming stream"));
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).expect("read streaming request");
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
        )
        .expect("write streaming headers");
        let events = [
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"do\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ne\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        ];
        for (index, event) in events.iter().enumerate() {
            if index > 0 {
                thread::sleep(Duration::from_millis(400));
            }
            write!(stream, "{:X}\r\n{}\r\n", event.len(), event).expect("write streaming event");
            stream.flush().expect("flush streaming event");
        }
        write!(stream, "0\r\n\r\n").expect("write streaming terminator");
    });

    let provider =
        test_provider_with_timeout(test_provider_config(format!("http://{address}/v1")), 1)
            .expect("provider")
            .with_selected_model(SelectedModel {
                model_name: "gpt-test".to_string(),
                api_protocol: ProviderApiProtocol::OpenAiResponses,
                max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                reasoning_variant: None,
                reasoning_enabled: false,
                wire_reasoning_effort: None,
                thinking_wire_format: ThinkingWireFormat::ThinkingType,
                tool_reasoning_mode: ProviderToolReasoningMode::DisabledForToolCalls,
                supports_developer_role: true,
                supports_tool_choice: true,
                requires_reasoning_content_for_tool_calls: false,
                requires_assistant_content_for_tool_calls: false,
            });
    let request = ModelTurnRequest::new(
        "slow_streaming_response",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let mut events = Vec::new();
    let response = provider
        .complete_stream(&request, &CancellationToken::new(), &mut |event| {
            events.push(event);
        })
        .expect("streaming response must survive a total duration above the idle timeout");

    assert_eq!(events.len(), 2);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .expect("assistant message")
            .content,
        "done"
    );
    server.join().expect("join slow streaming provider");
}

/// 契约透传：选中模型的 tool_reasoning_mode 必须反映到 protocol_contract()。
#[test]
fn protocol_contract_exposes_selected_tool_reasoning_mode() {
    let config = test_provider_config("http://127.0.0.1:1/v1".to_string());
    let provider = test_provider(config)
        .expect("provider")
        .with_selected_model(SelectedModel {
            model_name: "gpt-test".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_variant: Some("on".to_string()),
            reasoning_enabled: true,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ThinkingType,
            tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
            supports_developer_role: true,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: true,
            requires_assistant_content_for_tool_calls: false,
        });
    assert_eq!(
        provider.protocol_contract().tool_reasoning_mode,
        ProviderToolReasoningMode::ReplayReasoningContent
    );
}
