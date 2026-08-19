mod support;
use support::*;

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
    request.tool_choice.max_tool_calls = 1;
    request.tool_choice.strict_tool_schema = false;

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
    // 静态契约下 legacy provider 不声明 parallel/strict 支持，wire 投影跟随请求值。
    assert_eq!(captured["parallel_tool_calls"], false);
    assert!(
        captured["tools"][0]["function"].get("strict").is_none(),
        "strict field must be omitted when the request is not strict"
    );
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
    // 静态契约（parallel 未声明）下，超过 agent 上限的并行响应同时违反
    // parallel 支持声明，parallel 拒绝优先映射为 UnsupportedCapability。
    assert_eq!(
        occurrence.error_category,
        Some(ModelErrorCategory::UnsupportedCapability)
    );
    assert_eq!(
        occurrence.error_stage,
        Some(ProviderErrorStage::ResponseValidation)
    );
    assert_eq!(
        occurrence.diagnostic_code.as_deref(),
        Some("provider_does_not_support_parallel_tool_calls")
    );
    assert!(occurrence.usage.is_none());
    let error = response.error.expect("contract violation error");
    assert_eq!(error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.code.as_deref(),
        Some("provider_does_not_support_parallel_tool_calls")
    );
    assert_eq!(error.stage, Some(ProviderErrorStage::ResponseValidation));
    // 响应超限已降级为 warning（agent loop 按声明上限分窗口执行全部工具调用），
    // 不再作为致命校验；parallel 未声明时 parallel 拒绝仍是唯一致命校验。
    assert_eq!(
        error.validation_errors,
        vec!["provider_does_not_support_parallel_tool_calls"]
    );
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
                vec![(status_line, body); 6]
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
            assert_eq!(metadata.attempt_count, 6);
            assert_eq!(metadata.retry_count, 5);
        } else {
            assert_eq!(metadata.attempt_count, 1);
            assert_eq!(metadata.retry_count, 0);
        }
        assert!(!serialized.contains("provider body must not leak"));
        assert!(attempts.try_iter().count() >= 1);
    }
}

#[test]
fn openai_provider_recovers_native_argument_parse_errors_for_agent_repair() {
    for (case_name, arguments, expected_errors) in [
        ("invalid_json", "{\"path\":", vec!["invalid_json"]),
        (
            "non_object",
            "\"README.md\"",
            vec!["schema_mismatch", "tool_call_arguments_must_be_object"],
        ),
    ] {
        let body = serde_json::json!({
            "id": format!("resp_{case_name}"),
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": format!("call_{case_name}"),
                        "type": "function",
                        "function": {"name": "read", "arguments": arguments},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        })
        .to_string();
        let base_url = single_response_server("HTTP/1.1 200 OK", Box::leak(body.into_boxed_str()));
        let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
        let mut request = ModelTurnRequest::new(
            format!("request_{case_name}"),
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

        assert_eq!(response.status, ModelTurnStatus::Success, "{case_name}");
        assert!(response.error.is_none(), "{case_name}");
        assert_eq!(
            response.validation.as_ref().expect("validation").errors,
            expected_errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>(),
            "{case_name}"
        );
        assert_eq!(response.tool_calls.len(), 1, "{case_name}");
        assert_eq!(
            response.tool_calls[0].tool_call_id,
            format!("call_{case_name}")
        );
    }
}

#[test]
fn openai_provider_rejects_unknown_native_tool_even_when_arguments_are_repairable() {
    let body = r#"{
        "id": "resp_unknown_tool",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_unknown",
                    "type": "function",
                    "function": {"name": "unknown", "arguments": "{\"path\":"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;
    let base_url = single_response_server("HTTP/1.1 200 OK", body);
    let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
    let mut request = ModelTurnRequest::new(
        "request_unknown_tool",
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
    assert!(
        response
            .validation
            .as_ref()
            .expect("validation")
            .errors
            .contains(&"unknown_tool".to_string())
    );
    assert_eq!(
        response.error.as_ref().expect("validation error").kind,
        ModelErrorKind::JsonSchemaViolation
    );
}

#[test]
fn openai_provider_rejects_missing_or_non_string_chat_argument_fields() {
    for (case_name, arguments) in [("missing", None), ("number", Some(serde_json::json!(7)))] {
        let mut function = serde_json::json!({"name": "read"});
        if let Some(arguments) = arguments {
            function["arguments"] = arguments;
        }
        let body = serde_json::json!({
            "id": format!("resp_{case_name}_arguments"),
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": format!("call_{case_name}_arguments"),
                        "type": "function",
                        "function": function,
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        })
        .to_string();
        let base_url = single_response_server("HTTP/1.1 200 OK", Box::leak(body.into_boxed_str()));
        let provider = OpenAiProvider::new(provider_test_config(base_url)).expect("provider");
        let mut request = ModelTurnRequest::new(
            format!("request_{case_name}_arguments"),
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

        assert_eq!(response.status, ModelTurnStatus::Invalid, "{case_name}");
        assert!(response.error.is_some(), "{case_name}");
        assert!(
            response
                .validation
                .as_ref()
                .expect("validation")
                .errors
                .iter()
                .any(|error| error == "schema_mismatch"),
            "{case_name}"
        );
    }
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
            "provider_does_not_support_parallel_tool_calls"
        ]
    );
    // 超限（> 请求上限）只产生 warning，不阻止 agent 逐个执行。
    assert!(
        duplicate_result
            .warnings
            .contains(&"max_tool_calls_exceeded".to_string())
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

/// Chat 最终响应：含 reasoning_content 但没有 tool call（reasoning-only）。
const CHAT_REASONING_ONLY_RESPONSE: &str = r#"{
    "id": "chat_reasoning_only",
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "reasoned answer",
            "reasoning_content": "private reasoning without tool call"
        },
        "finish_reason": "stop"
    }]
}"#;

/// 矩阵项 1：Chat non-stream，请求带 tool schema 且模式为 ReplayReasoningContent；
/// 响应仅含 reasoning_content 而无 tool call → 成功，assistant content 保留，
/// `provider_reasoning_history` 为空（不产生 replay 义务）。
#[test]
fn reasoning_replay_obligation_chat_reasoning_only_response_is_legal_without_replay() {
    let (base_url, request_body) =
        captured_request_server("HTTP/1.1 200 OK", CHAT_REASONING_ONLY_RESPONSE);
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_reasoning_content_for_tool_calls": true,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("reasoning-only Chat response must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("reasoned answer")
    );
    assert!(response.tool_calls.is_empty());
    assert!(
        response.provider_reasoning_history.is_empty(),
        "reasoning-only final answer must not create a replay obligation"
    );
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Chat request"),
    )
    .expect("Chat payload JSON");
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert_eq!(payload["reasoning_effort"], "high");
}

/// 矩阵项 2：Chat non-stream，ReplayReasoningContent 模式，响应含
/// reasoning_content + tool call，且 parse 生成合法 Chat replay → 成功。
#[test]
fn reasoning_replay_obligation_chat_tool_call_with_replay_succeeds() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{
            "id": "chat_reasoning_tool_call",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": "opaque chain of thought",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_reasoning_content_for_tool_calls": true,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Chat tool call with replay must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_call_id, "call_1");
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(
        response.tool_calls[0].parse_status,
        ModelToolParseStatus::Valid
    );
    assert_eq!(response.provider_reasoning_history.len(), 1);
    match &response.provider_reasoning_history[0] {
        ProviderReasoningReplay::Chat {
            tool_call_ids,
            reasoning_content,
            ..
        } => {
            assert_eq!(tool_call_ids, &vec!["call_1".to_string()]);
            assert_eq!(reasoning_content, "opaque chain of thought");
        }
        other => panic!("expected Chat replay, got {other:?}"),
    }
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Chat request"),
    )
    .expect("Chat payload JSON");
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert_eq!(payload["reasoning_effort"], "high");
}

/// 矩阵项 3：Chat non-stream，ReplayReasoningContent 模式，响应含真实 tool call
/// 但没有 reasoning_content → parse 不产生 replay → typed fail closed
/// （`provider_tool_reasoning_history_unsupported` /
/// `tool_reasoning_content_requires_adapter_history_support`）。
#[test]
fn reasoning_replay_obligation_chat_tool_call_without_replay_fails_closed() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{
            "id": "chat_tool_call_no_reasoning",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_reasoning_content_for_tool_calls": true,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let error = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect_err("Chat tool call without replay must fail closed");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
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
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Chat request"),
    )
    .expect("Chat payload JSON");
    assert_eq!(payload["thinking"]["type"], "enabled");
}

/// 矩阵项 3b：Chat non-stream，ReplayReasoningContent 模式（未声明
/// `requires_reasoning_content_for_tool_calls`，即 flag=false），响应含 tool call
/// 但无 `reasoning_content` → 成功（thinking 模式不保证每轮输出 reasoning，
/// 无 reasoning 即无可回放内容；真实 deepseek-v4-flash#high 续接轮已观察到）。
#[test]
fn reasoning_replay_obligation_chat_tool_call_without_reasoning_succeeds() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{
            "id": "chat_tool_call_no_reasoning",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Chat tool call without reasoning must succeed when not required");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert!(response.provider_reasoning_history.is_empty());
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Chat request"),
    )
    .expect("Chat payload JSON");
    assert_eq!(payload["thinking"]["type"], "enabled");
}

/// 矩阵项 3c：Chat 请求侧，Replay 模式拒绝没有对应 assistant tool-call
/// 消息的 orphan replay，即使模型没有声明每轮都必须返回 reasoning。
#[test]
fn reasoning_replay_obligation_chat_request_orphan_replay_fails_closed() {
    let (base_url, _request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{
            "id": "chat_final_answer",
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let mut request = ModelTurnRequest::new(
        "reasoning_chat_request",
        vec![
            ModelMessage::text(ModelRole::Developer, "instruction"),
            ModelMessage::assistant_tool_calls(vec![tool_call("call_2", "read")]),
        ],
    );
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
        provider_name: "reasoning_test".to_string(),
        model_name: "chat".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec!["call_1".to_string()],
        reasoning_content: "opaque-deepseek-state".to_string(),
    }];
    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("orphan replay must fail closed");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_tool_reasoning_history_unsupported")
    );
}

/// 矩阵项 3d：Chat 请求侧，flag=true（严格声明）时每个 tool-call 消息仍必须
/// 有绑定 reasoning 的 replay；历史里只有另一组 ids 的 replay 时 fail-closed。
#[test]
fn reasoning_replay_obligation_chat_request_tool_call_without_matching_replay_fails_closed_strict()
{
    let (base_url, _request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{
            "id": "chat_final_answer",
            "choices": [{
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/chat#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "chat": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "reasoning_content",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_reasoning_content_for_tool_calls": true,
                            "requires_assistant_content_for_tool_calls": true
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/chat#high"))
        .expect("selected Chat provider");
    let mut request = ModelTurnRequest::new(
        "reasoning_chat_request",
        vec![
            ModelMessage::text(ModelRole::Developer, "instruction"),
            ModelMessage::assistant_tool_calls(vec![tool_call("call_2", "read")]),
        ],
    );
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
        provider_name: "reasoning_test".to_string(),
        model_name: "chat".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec!["call_1".to_string()],
        reasoning_content: "opaque-deepseek-state".to_string(),
    }];
    let error = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect_err("strict request without bound reasoning must fail closed");
    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
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
}

/// 矩阵项 4：Responses non-stream，ReplayResponsesItems 模式，响应含 reasoning
/// item + 最终 message、无 function call → 成功且无孤立 replay
/// （`provider_reasoning_history` 为空）。
#[test]
fn reasoning_replay_obligation_responses_reasoning_only_is_legal_without_replay() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_reasoning_only",
        "object": "response",
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            }
        ]
    }));
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("reasoning-only Responses response must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("done")
    );
    assert!(response.tool_calls.is_empty());
    assert!(
        response.provider_reasoning_history.is_empty(),
        "reasoning-only final answer must not create an orphan replay"
    );
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Responses requests");
    let payload: serde_json::Value =
        serde_json::from_str(&captured.last().expect("actual Responses request").1)
            .expect("Responses payload JSON");
    assert_eq!(payload["reasoning"]["effort"], "high");
}

/// 矩阵项 5：Responses stream，同一 reasoning-only、无 function call 响应 → 成功。
#[test]
fn reasoning_replay_obligation_responses_stream_reasoning_only_is_legal() {
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "response_stream_reasoning_only",
            "object": "response",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                }
            ]
        }
    });
    let body = format!("event: response.completed\r\ndata: {completed}\r\n\r\n");
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let mut events = Vec::new();
    let response = provider
        .complete_stream(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect("reasoning-only Responses stream must be accepted");
    assert!(events.is_empty());
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(
        response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str()),
        Some("done")
    );
    assert!(response.tool_calls.is_empty());
    assert!(
        response.provider_reasoning_history.is_empty(),
        "reasoning-only stream must not create an orphan replay"
    );
    let payload: serde_json::Value = serde_json::from_str(
        &requests
            .recv_timeout(Duration::from_secs(1))
            .expect("Responses stream request"),
    )
    .expect("Responses stream payload JSON");
    assert_eq!(payload["stream"], true);
}

/// 矩阵项 6（non-stream）：Responses，ReplayResponsesItems 模式，有 function call
/// 且带合法 replay（reasoning item 随附）→ 成功且 replay 被保留。
#[test]
fn reasoning_replay_obligation_responses_tool_call_with_replay_succeeds() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_reasoning_tool_call",
        "object": "response",
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            }
        ]
    }));
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Responses function call with replay must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_call_id, "call_1");
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(
        response.tool_calls[0].parse_status,
        ModelToolParseStatus::Valid
    );
    assert_eq!(response.provider_reasoning_history.len(), 1);
    match &response.provider_reasoning_history[0] {
        ProviderReasoningReplay::Responses {
            tool_call_ids,
            items,
            ..
        } => {
            assert_eq!(tool_call_ids, &vec!["call_1".to_string()]);
            assert!(items.iter().any(|item| item["type"] == "reasoning"));
            assert!(items.iter().any(|item| item["type"] == "function_call"));
        }
        other => panic!("expected Responses replay, got {other:?}"),
    }
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Responses requests");
    let payload: serde_json::Value =
        serde_json::from_str(&captured.last().expect("actual Responses request").1)
            .expect("Responses payload JSON");
    assert_eq!(payload["reasoning"]["effort"], "high");
}

/// 矩阵项 6（non-stream）：Responses，有合法 function call 但未返回 reasoning item →
/// 成功且不合成 provider replay。
#[test]
fn reasoning_replay_obligation_responses_tool_call_without_replay_succeeds() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_tool_call_no_reasoning",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            }
        ]
    }));
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let response = provider
        .complete(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Responses function call without reasoning must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_call_id, "call_1");
    assert!(response.provider_reasoning_history.is_empty());
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Responses requests");
    let payload: serde_json::Value =
        serde_json::from_str(&captured.last().expect("actual Responses request").1)
            .expect("Responses payload JSON");
    assert_eq!(payload["reasoning"]["effort"], "high");
}

/// 矩阵项 6（stream）：Responses stream，有 function call 且带合法 replay → 成功。
#[test]
fn reasoning_replay_obligation_responses_stream_tool_call_with_replay_succeeds() {
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "response_stream_reasoning_tool_call",
            "object": "response",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{}"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                }
            ]
        }
    });
    let body = format!("event: response.completed\r\ndata: {completed}\r\n\r\n");
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let mut events = Vec::new();
    let response = provider
        .complete_stream(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .expect("Responses stream function call with replay must be accepted");
    assert!(events.is_empty());
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_call_id, "call_1");
    assert_eq!(response.tool_calls[0].tool_name, "read");
    assert_eq!(response.provider_reasoning_history.len(), 1);
    match &response.provider_reasoning_history[0] {
        ProviderReasoningReplay::Responses {
            tool_call_ids,
            items,
            ..
        } => {
            assert_eq!(tool_call_ids, &vec!["call_1".to_string()]);
            assert!(items.iter().any(|item| item["type"] == "reasoning"));
        }
        other => panic!("expected Responses replay, got {other:?}"),
    }
    let payload: serde_json::Value = serde_json::from_str(
        &requests
            .recv_timeout(Duration::from_secs(1))
            .expect("Responses stream request"),
    )
    .expect("Responses stream payload JSON");
    assert_eq!(payload["stream"], true);
}

/// 矩阵项 6（stream）：Responses stream，有合法 function call 但未返回 reasoning item →
/// 成功且不合成 provider replay。
#[test]
fn reasoning_replay_obligation_responses_stream_tool_call_without_replay_succeeds() {
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": "response_stream_tool_call_no_reasoning",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{}"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                }
            ]
        }
    });
    let body = format!("event: response.completed\r\ndata: {completed}\r\n\r\n");
    let chunks = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| chunk.to_vec())
        .collect();
    let (base_url, requests) = responses_stream_server(chunks, None);
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "reasoning_test/responses#high",
            "providers": {
                "reasoning_test": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "REASONING_TEST_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true, "wire_effort": "high"}
                            },
                            "default_variant": "high",
                            "tool_reasoning_history": "responses_items"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "REASONING_TEST_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        None,
    );
    let provider = snapshot
        .provider_for_selector(Some("reasoning_test/responses#high"))
        .expect("selected Responses provider");
    let response = provider
        .complete_stream(
            &capability_test_request(None, false, 1),
            &singularity_core::CancellationToken::new(),
            &mut |_| {},
        )
        .expect("Responses stream function call without reasoning must be accepted");
    assert_eq!(response.status, ModelTurnStatus::Success);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_call_id, "call_1");
    assert!(response.provider_reasoning_history.is_empty());
    let payload: serde_json::Value = serde_json::from_str(
        &requests
            .recv_timeout(Duration::from_secs(1))
            .expect("Responses stream request"),
    )
    .expect("Responses stream payload JSON");
    assert_eq!(payload["stream"], true);
}
