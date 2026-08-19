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
            "",
        ),
        (
            "failed",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            "responses_stream_failed",
            "",
        ),
        (
            "incomplete",
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
            "responses_stream_incomplete",
            "max_output_tokens",
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
        if !expected_fragment.is_empty() {
            assert!(
                error.error.message.contains(expected_fragment),
                "{name}: message must carry the incomplete reason"
            );
        }
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
    assert!(
        response.provider_reasoning_history.is_empty(),
        "Responses tool calls without reasoning must not synthesize an off replay"
    );
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
            &capability_test_request(None, false, 1),
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
fn openai_chat_response_wire_discriminators_fail_closed_before_normalization() {
    let cases = [
        (
            "role",
            r#"{
                "id": "chat_invalid_role",
                "choices": [{
                    "message": {
                        "role": "user",
                        "content": "ignored",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
            "chat_message_role_invalid",
            true,
        ),
        (
            "tool type",
            r#"{
                "id": "chat_invalid_tool_type",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "custom",
                            "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
            "chat_tool_call_type_invalid",
            true,
        ),
        (
            "content part",
            r#"{
                "id": "chat_invalid_content_part",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "image_url", "image_url": {"url": "https://example.invalid"}}],
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
            "chat_content_part_type_invalid",
            true,
        ),
        (
            "length",
            r#"{
                "id": "chat_length_with_tool_call",
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
                    "finish_reason": "length"
                }]
            }"#,
            "chat_completion_incomplete",
            true,
        ),
        (
            "content filter",
            r#"{
                "id": "chat_filter_with_tool_call",
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
                    "finish_reason": "content_filter"
                }]
            }"#,
            "chat_completion_incomplete",
            true,
        ),
        (
            "length without tools",
            r#"{
                "id": "chat_incomplete_text",
                "choices": [{
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": "length"
                }]
            }"#,
            "chat_completion_incomplete",
            false,
        ),
        (
            "content filter without tools",
            r#"{
                "id": "chat_incomplete_text",
                "choices": [{
                    "message": {"role": "assistant", "content": "partial"},
                    "finish_reason": "content_filter"
                }]
            }"#,
            "chat_completion_incomplete",
            false,
        ),
    ];

    for (case_name, body, expected_code, with_tools) in cases {
        let base_url = single_response_server("HTTP/1.1 200 OK", body);
        let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
        let request = if with_tools {
            capability_test_request(None, false, 1)
        } else {
            ModelTurnRequest::new(
                "chat_incomplete_text",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            )
        };
        let error = provider
            .complete(&request, &singularity_core::CancellationToken::new())
            .expect_err("malformed Chat response must fail closed");

        assert_eq!(
            error.error.kind,
            ModelErrorKind::JsonSchemaViolation,
            "{case_name}"
        );
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_response_invalid"),
            "{case_name}"
        );
        assert_eq!(
            error.error.stage,
            Some(ProviderErrorStage::ResponseValidation),
            "{case_name}"
        );
        assert_eq!(
            error.error.validation_errors,
            vec![expected_code.to_string()],
            "{case_name}"
        );
    }
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
fn openai_provider_honors_retry_after_ms_before_retrying() {
    let success_body = r#"{
        "id": "resp_retry_header",
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }]
    }"#;
    let (base_url, attempts) = sequence_response_server_with_headers(vec![
        (
            "HTTP/1.1 429 Too Many Requests",
            "{}",
            "retry-after-ms: 200\r\nretry-after: 9\r\n",
        ),
        ("HTTP/1.1 200 OK", success_body, ""),
    ]);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let request = ModelTurnRequest::new(
        "request_retry_header",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );

    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("provider response after header-directed retry");
    let metadata = response
        .provider_attempt_metadata
        .expect("attempt metadata");

    assert_eq!(metadata.retry_count, 1);
    assert_eq!(metadata.occurrences[0].retry_backoff_ms, Some(200));
    assert_eq!(attempts.iter().collect::<Vec<_>>(), vec![1, 2]);
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
        for _ in 0..6 {
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
    assert_eq!(metadata.attempt_count, 6);
    assert_eq!(metadata.retry_count, 5);
    assert!(metadata.latency_ms >= 1550);
    assert_eq!(metadata.occurrences.len(), 6);
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
        assert_eq!(occurrence.retry_scheduled, index < 5);
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

    assert_eq!(metadata.attempt_count, 6);
    assert_eq!(metadata.retry_count, 5);
    assert_eq!(metadata.occurrences.len(), 6);
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
        assert_eq!(occurrence.retry_scheduled, index < 5);
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
