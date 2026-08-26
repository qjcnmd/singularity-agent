mod support;
use support::*;

#[test]
fn openai_responses_stream_aggregates_deltas_and_ignores_ping_after_completion() {
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
        "event: response.output_text.delta\r\ndata: {delta_one}\r\n\r\nevent: response.output_text.delta\r\ndata: {delta_two}\r\n\r\nevent: response.completed\r\ndata: {completed}\r\n\r\nevent: ping\r\ndata: {{\"type\":\"ping\"}}\r\n\r\n"
    );
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let provider = test_provider(provider_config_with_base_url(base_url)).expect("provider");
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
}

#[test]
fn openai_responses_stream_maps_terminal_failures_and_protocol_failures() {
    let cases = [
        (
            "error",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"provider_error\",\"message\":\"secret raw failure\"}}\n\n",
            "responses_stream_error",
            "",
        ),
        (
            "failed",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            "responses_stream_failed",
            "",
        ),
        (
            "missing_terminal",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "responses_stream_terminal_missing",
            "",
        ),
        (
            "malformed",
            "event: response.output_text.delta\ndata: {not-json}\n\n",
            "responses_stream_malformed",
            "",
        ),
        (
            "business_event_after_terminal",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n",
            "responses_stream_malformed",
            "",
        ),
    ];
    for (name, body, expected_code, expected_fragment) in cases {
        let chunks = body
            .as_bytes()
            .chunks(2)
            .map(|chunk| chunk.to_vec())
            .collect();
        let (base_url, requests) = responses_stream_server(chunks, None);
        let provider = test_provider(provider_config_with_base_url(base_url)).expect("provider");
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
        if !expected_fragment.is_empty() {
            assert!(
                error.error.message.contains(expected_fragment),
                "{name}: message must carry the incomplete reason"
            );
        }
        requests
            .recv_timeout(Duration::from_secs(1))
            .expect("stream request was sent");
    }
}

#[test]
fn openai_responses_stream_length_preserves_partial_response() {
    let body = concat!(
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_incomplete\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]},{\"type\":\"function_call\",\"call_id\":\"call_read\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n"
    );
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let provider = test_provider(provider_config_with_base_url(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_stream_incomplete",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: json!({"type":"object"}),
    });
    let response = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect("max_output_tokens incomplete stream remains partial success");
    assert!(response.is_length_truncated());
    assert_eq!(response.finish_reason.as_deref(), Some("length"));
    assert_eq!(
        response.assistant_message.as_ref().unwrap().content,
        "partial"
    );
    assert_eq!(response.tool_calls().len(), 1);
    assert_eq!(response.tool_calls()[0].tool_call_id, "call_read");
    assert_eq!(response.usage.total_tokens, 7);
    requests
        .recv_timeout(Duration::from_secs(1))
        .expect("incomplete stream request was sent");
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
        )
        .expect_err("oversized stream must fail closed");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_stream_too_large")
    );
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
    requests
        .recv_timeout(Duration::from_secs(1))
        .expect("tool stream request");
}

#[test]
fn openai_chat_streaming_normalizes_visible_deltas_and_tool_fragments() {
    let body = concat!(
        "data: {\"id\":\"chat_stream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, requests) = chat_stream_server(
        body.as_bytes()
            .chunks(7)
            .map(|chunk| chunk.to_vec())
            .collect(),
        None,
    );
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_chat_stream",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.tools.push(ModelToolSchema {
        name: "read".to_string(),
        description: "Read a file".to_string(),
        parameters_schema: json!({"type":"object"}),
    });
    let mut events = Vec::new();
    let response = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect("Chat stream completion");
    assert_eq!(
        events,
        vec![
            ProviderStreamEvent::OutputTextDelta {
                delta: "你".into()
            },
            ProviderStreamEvent::OutputTextDelta {
                delta: "好".into()
            }
        ]
    );
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response.stop_reason(),
        Some(singularity_model::ModelStopReason::Stop)
    );
    assert_eq!(response.tool_calls()[0].tool_name, "read");
    assert_eq!(
        response.tool_calls()[0].raw_arguments,
        r#"{"path":"README.md"}"#
    );
    assert_eq!(response.usage.total_tokens, 7);
    let body = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("Chat request");
    let payload: Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(payload["stream"], true);
    assert_eq!(payload["stream_options"]["include_usage"], true);
}

#[test]
fn streaming_capability_is_bound_to_the_selected_protocol() {
    assert_eq!(
        ProviderStreamingCapability::for_protocol(ProviderApiProtocol::OpenAiResponses),
        ProviderStreamingCapability::OutputTextDelta
    );
    assert_eq!(
        ProviderStreamingCapability::for_protocol(ProviderApiProtocol::OpenAiChatCompletions),
        ProviderStreamingCapability::OutputTextDelta
    );
    assert_eq!(
        ProviderStreamingCapability::for_protocol(ProviderApiProtocol::Declared),
        ProviderStreamingCapability::Unsupported
    );

    let provider = test_provider(provider_config_with_base_url(
        "http://127.0.0.1:1/v1/responses".to_string(),
    ))
    .expect("provider");
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::OpenAiResponses),
        ProviderStreamingCapability::OutputTextDelta
    );
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::OpenAiChatCompletions),
        ProviderStreamingCapability::OutputTextDelta
    );
    assert_eq!(
        provider.streaming_capability(ProviderApiProtocol::Declared),
        ProviderStreamingCapability::Unsupported
    );
}

#[test]
fn openai_responses_stream_marks_retry_safety_around_first_text_delta() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stream retry provider");
    let address = listener
        .local_addr()
        .expect("stream retry provider address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept stream retry request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        read_provider_request(&mut reader);
        let body = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len() + 16,
            body
        )
        .expect("write truncated stream body");
    });
    let provider = test_provider(provider_config_with_base_url(format!(
        "http://{address}/v1/responses"
    )))
    .expect("provider");
    let request = ModelTurnRequest::new(
        "request_stream_retry_before_delta",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();
    let error = provider
        .complete_stream(
            &request,
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect_err("the transport returns the first stream failure");
    assert!(error.automatic_retry_allowed);
    assert_eq!(events.len(), 0);
    server.join().expect("join stream server");

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
    let provider = test_provider(provider_config_with_base_url(format!(
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
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    assert!(!error.automatic_retry_allowed);
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
    let provider = test_provider(provider_config_with_base_url(format!(
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
    worker.join().expect("join stream cancellation worker");
    server.join().expect("join stream cancellation server");
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
    let provider = test_provider(provider_auto_test_config(base_url)).expect("provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("invalid provider response remains observable");

    assert_eq!(response.status, ModelTurnStatus::Invalid);
    assert!(response.tool_calls().is_empty());
    assert_eq!(
        response.validation.expect("response validation").errors,
        vec!["text_tool_call_envelope_not_supported"]
    );
    assert_eq!(
        requests
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Responses requests")
            .len(),
        1
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
    assert_eq!(
        response.tool_calls()[0].arguments,
        serde_json::json!({"path": "README.md"})
    );
    assert!(!serialized.contains("test-key-placeholder"));
    assert!(!serialized.contains("choices"));
}

#[test]
fn openai_chat_request_output_cap_remains_on_wire() {
    let (base_url, requests) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"request_cap","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_cap",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    request.model_preferences.max_output_tokens = Some(123);
    provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("request cap is within provider capability");
    let body = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured request body");
    let payload: Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(payload["max_tokens"], 123);
}

#[test]
fn openai_chat_response_wire_discriminators_fail_closed_before_normalization() {
    let cases = [
        (
            r#"{"id":"chat_invalid_role","choices":[{"message":{"role":"user","content":"ignored"},"finish_reason":"tool_calls"}]}"#,
            "chat_message_role_invalid",
        ),
        (
            r#"{"id":"chat_invalid_tool_type","choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"custom","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            "chat_tool_call_type_invalid",
        ),
        (
            r#"{"id":"chat_invalid_content_part","choices":[{"message":{"role":"assistant","content":[{"type":"image_url"}]},"finish_reason":"stop"}]}"#,
            "chat_content_part_type_invalid",
        ),
    ];
    for (body, expected_code) in cases {
        let base_url = single_response_server("HTTP/1.1 200 OK", body);
        let provider = test_provider(provider_test_config(base_url)).expect("provider");
        let error = provider
            .complete(
                &ModelTurnRequest::new(
                    "chat_invalid",
                    vec![ModelMessage::text(ModelRole::User, "hello")],
                ),
                &singularity_core::CancellationToken::new(),
            )
            .expect_err("malformed Chat response must fail closed");
        assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_response_invalid")
        );
        assert_eq!(
            error.error.validation_errors,
            vec![expected_code.to_string()]
        );
    }
}

#[test]
fn openai_chat_length_preserves_partial_and_content_filter_is_typed() {
    let length_body = r#"{
        "id":"chat_length_with_tool_call",
        "choices":[{"message":{"role":"assistant","content":"partial","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}]},"finish_reason":"length"}],
        "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", length_body);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let mut request = capability_test_request(None, false, 1);
    request.request_id = "chat_length_with_tool_call".to_string();
    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("length response remains a normal partial completion");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert!(response.is_length_truncated());
    assert_eq!(response.finish_reason.as_deref(), Some("length"));
    assert_eq!(response.tool_calls().len(), 1);
    assert_eq!(response.usage.total_tokens, 7);

    let filter_body = r#"{"id":"chat_filter","choices":[{"message":{"role":"assistant","content":"blocked"},"finish_reason":"content_filter"}]}"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", filter_body);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let error = provider
        .complete(
            &ModelTurnRequest::new(
                "chat_filter",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("content filter remains a typed error");
    assert_eq!(error.error.kind, ModelErrorKind::ContentFilter);
    assert_eq!(error.error.code.as_deref(), Some("content_filter"));
}

#[test]
fn openai_provider_returns_transient_http_error_after_one_attempt() {
    let (base_url, attempts) =
        sequence_response_server(vec![("HTTP/1.1 429 Too Many Requests", "{}")]);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("transport owns exactly one HTTP attempt");
    assert_eq!(error.error.kind, ModelErrorKind::RateLimited);
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1]);
}

#[test]
fn openai_provider_carries_retry_after_to_the_caller() {
    let (base_url, attempts) = sequence_response_server_with_headers(vec![(
        "HTTP/1.1 429 Too Many Requests",
        "{}",
        "retry-after-ms: 200\r\nretry-after: 9\r\n",
    )]);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_retry_header",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("transport returns the first transient failure");
    assert_eq!(error.retry_after, Some(Duration::from_millis(200)));
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1]);
}

#[test]
fn openai_provider_observes_one_ordered_start_end_pair() {
    let (base_url, attempts) =
        sequence_response_server(vec![("HTTP/1.1 429 Too Many Requests", "{}")]);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_observed_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();

    let error = Provider::complete_observed(
        &provider,
        &request,
        &singularity_core::CancellationToken::new(),
        &mut |event| {
            events.push(event);
        },
    )
    .expect_err("one observed attempt returns its typed error");

    assert_eq!(error.error.kind, ModelErrorKind::RateLimited);
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1]);
    let [
        ProviderAttemptEvent::Started(started),
        ProviderAttemptEvent::Finished(finished),
    ] = events.as_slice()
    else {
        panic!("one ordered start/end pair expected");
    };
    assert_eq!(started.provider_name, finished.provider_name);
    assert_eq!(started.model_name, finished.model_name);
    assert_eq!(started.actual_api_protocol, finished.actual_api_protocol);
    assert_eq!(started.started_at_unix_ms, finished.started_at_unix_ms);
}

#[test]
fn openai_provider_delivers_observed_attempts_without_rejecting() {
    // 观察端尽力而为：无拒绝路径，HTTP 请求照常进行；Started 先于
    // Finished 送达，完成不因观察端行为被丢弃。
    let success_body = r#"{
        "id": "resp_observed",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server(vec![("HTTP/1.1 200 OK", success_body)]);
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_observed_complete",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let mut events = Vec::new();

    let response = Provider::complete_observed(
        &provider,
        &request,
        &singularity_core::CancellationToken::new(),
        &mut |event| {
            events.push(event);
        },
    )
    .expect("observed attempt completes");

    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1]);
    assert_eq!(response.status, ModelTurnStatus::Success);
    let [
        ProviderAttemptEvent::Started(_),
        ProviderAttemptEvent::Finished(_),
    ] = events.as_slice()
    else {
        panic!("one ordered start/end pair expected");
    };
}

#[test]
fn openai_provider_uses_external_runtime_handle_for_http_body() {
    let success_body = r#"{
        "id": "resp_external_runtime",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server(vec![("HTTP/1.1 200 OK", success_body)]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("external Tokio runtime");
    let provider = OpenAiProvider::new(provider_test_config(base_url), runtime.handle().clone())
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
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1]);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

#[test]
fn openai_provider_returns_body_transport_failure_after_one_attempt() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind truncated provider");
    let address = listener.local_addr().expect("truncated provider address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept provider request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
        read_provider_request(&mut reader);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\nconnection: close\r\n\r\n{}",
            )
            .expect("write truncated provider response");
    });
    let provider =
        test_provider(provider_test_config(format!("http://{address}"))).expect("provider");
    let request = ModelTurnRequest::new(
        "request_transport_retry",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("transport failure");
    assert_eq!(error.error.kind, ModelErrorKind::NetworkError);
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ResponseBodyRead)
    );
    server.join().expect("join truncated provider");
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
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("closed address must fail during send");
    let serialized = serde_json::to_string(&error.error).expect("serialize error");
    assert!(!serialized.contains("sensitive prompt marker"));
    assert!(!serialized.contains("test-key-placeholder"));
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
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_multiple_choices",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("multiple choices must be rejected");
    assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_response_invalid")
    );
    assert_eq!(
        error.error.validation_errors,
        vec!["response_choices_count_invalid"]
    );
    assert!(!error.automatic_retry_allowed);
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
        test_provider(provider_test_config(format!("http://{address}"))).expect("provider");
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
