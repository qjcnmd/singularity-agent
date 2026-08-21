mod support;
use support::*;

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
fn model_discovery_rejects_any_invalid_model_entry() {
    let invalid_payloads = [
        ("missing id", r#"{"data":[{"id":"gpt-valid"},{}]}"#),
        ("empty id", r#"{"data":[{"id":"gpt-valid"},{"id":""}]}"#),
        (
            "whitespace id",
            r#"{"data":[{"id":"gpt-valid"},{"id":"gpt invalid"}]}"#,
        ),
        (
            "control character id",
            r#"{"data":[{"id":"gpt-valid"},{"id":"gpt\ninvalid"}]}"#,
        ),
        (
            "duplicate id",
            r#"{"data":[{"id":"gpt-valid"},{"id":"gpt-valid"}]}"#,
        ),
    ];
    for (label, payload) in invalid_payloads {
        let (base_url, request) = models_server(payload.to_string());
        let provider = test_provider(provider_auto_test_config(base_url)).expect("models provider");
        let error = provider.discover_model_ids().expect_err(label);
        assert_eq!(
            error.error.kind,
            ModelErrorKind::JsonSchemaViolation,
            "{label}"
        );
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_models_schema_invalid"),
            "{label}"
        );
        assert_eq!(
            error.error.stage,
            Some(ProviderErrorStage::ResponseValidation),
            "{label}"
        );
        assert!(
            request
                .recv_timeout(Duration::from_secs(1))
                .expect("models request")
                .contains("GET /v1/models"),
            "{label}"
        );
    }
}

#[test]
fn model_discovery_accepts_complete_unique_model_entries() {
    let (base_url, request) =
        models_server(r#"{"data":[{"id":"gpt-test"},{"id":"o4-mini"}]}"#.to_string());
    let provider = test_provider(provider_auto_test_config(base_url)).expect("models provider");
    assert_eq!(
        provider.discover_model_ids().expect("complete catalog"),
        vec!["gpt-test", "o4-mini"]
    );
    assert!(
        request
            .recv_timeout(Duration::from_secs(1))
            .expect("models request")
            .contains("GET /v1/models")
    );
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
    let snapshot = ProviderConfigSnapshot::capture(
        |name| {
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
        },
        test_runtime_handle(),
    );

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

    let same_config = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("snapshot-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("https://snapshot-provider.example/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("snapshot-secret".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    assert_ne!(snapshot.snapshot_id(), same_config.snapshot_id());
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
        let snapshot = ProviderConfigSnapshot::capture(
            |candidate| match candidate {
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
            },
            test_runtime_handle(),
        );

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
    let malformed = test_provider(provider_test_config(malformed_url)).expect("malformed provider");
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
    let missing_choices =
        test_provider(provider_test_config(missing_choices_url)).expect("missing choices provider");
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
        Some(DEFAULT_MAX_CONTEXT_TOKENS)
    );
    assert_eq!(
        default_config.protocol_contract().max_output_tokens,
        DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert!(
        default_config
            .protocol_contract()
            .supports_developer_message
    );
    assert!(default_config.protocol_contract().supports_system_message);
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
    assert_eq!(capabilities.max_context_tokens, Some(131_072));
    assert_eq!(capabilities.max_output_tokens, 8_192);
    assert!(!capabilities.supports_parallel_tool_calls);
    assert!(!capabilities.supports_strict_tool_schema);

    let provider = test_provider(configured).expect("provider");
    assert_eq!(Provider::protocol_contract(&provider), capabilities);
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
    request.tool_choice.strict_tool_schema = true;
    let capabilities = ProviderProtocolContract {
        supports_tools: false,
        supports_developer_message: false,
        ..ProviderProtocolContract::default()
    };

    let result = validate_model_request_with_capabilities(&request, Some(&capabilities));

    assert_eq!(
        result.errors,
        vec![
            "provider_does_not_support_developer_messages",
            "provider_does_not_support_strict_tool_schema",
            "provider_does_not_support_tools",
        ]
    );
}

#[test]
fn openai_provider_debug_redacts_secret_configuration() {
    let config = provider_test_config("https://provider.example/v1".to_string());
    let provider = test_provider(config.clone()).expect("provider");
    let config_debug = format!("{config:?}");
    let provider_debug = format!("{provider:?}");

    for debug_text in [config_debug, provider_debug] {
        assert!(!debug_text.contains("sk-secret-value"));
        assert!(!debug_text.contains("provider.example"));
        assert!(debug_text.contains("[redacted]"));
    }
}

#[test]
fn model_catalog_captures_once_and_resolves_fixed_protocols_and_limits() {
    let directory = tempdir().expect("model catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "first/chat-model",
            "providers": {
                "first": {
                    "adapter": "openai_compatible",
                    "base_url": "https://first.example/v1",
                    "api_key_env": "FIRST_KEY",
                    "models": {
                        "chat-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000
                        }
                    }
                },
                "second": {
                    "adapter": "openai_compatible",
                    "base_url": "https://second.example/v1",
                    "api_key_env": "SECOND_KEY",
                    "models": {
                        "responses_model": {
                            "api_protocol": "responses",
                            "max_context_tokens": 200000,
                            "max_output_tokens": 100000
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write model catalog");
    let config_path = config_path.to_string_lossy().into_owned();
    let mut reads = std::collections::HashMap::<String, usize>::new();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| {
            let count = reads.entry(name.to_string()).or_default();
            *count += 1;
            assert_eq!(*count, 1, "configuration value {name} was captured twice");
            match name {
                "SINGULARITY_MODELS_CONFIG" => Some(config_path.clone()),
                "FIRST_KEY" => Some("first-secret".to_string()),
                "SECOND_KEY" => Some("second-secret".to_string()),
                _ => None,
            }
        },
        test_runtime_handle(),
    );

    assert!(snapshot.configuration().configured);
    assert_eq!(
        snapshot.redacted_config().model_name.as_deref(),
        Some("first/chat-model")
    );
    let chat = snapshot
        .provider_for_selector(Some("first/chat-model"))
        .expect("chat selection");
    assert_eq!(
        chat.selected_api_protocol(),
        Some(ProviderApiProtocol::OpenAiChatCompletions)
    );
    assert_eq!(chat.protocol_contract().max_context_tokens, Some(1_000_000));
    assert_eq!(chat.protocol_contract().max_output_tokens, 384_000);
    let unsupported_off = snapshot
        .provider_for_selector(Some("first/chat-model#off"))
        .expect_err("#off must not be exposed without an explicit thinking contract");
    assert_eq!(
        unsupported_off.error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
    );
    let responses = snapshot
        .provider_for_selector(Some("second/responses_model"))
        .expect("responses selection");
    assert_eq!(
        responses.selected_api_protocol(),
        Some(ProviderApiProtocol::OpenAiResponses)
    );
    assert_eq!(responses.protocol_contract().max_output_tokens, 100_000);
    let unknown = snapshot
        .provider_for_selector(Some("second/not-allowlisted"))
        .expect_err("unknown model must fail closed");
    assert_eq!(
        unknown.error.code.as_deref(),
        Some("provider_selector_unknown_model")
    );
    let malformed = snapshot
        .provider_for_selector(Some("bare-model"))
        .expect_err("bare catalog selector must fail closed");
    assert_eq!(
        malformed.error.code.as_deref(),
        Some("provider_selector_invalid")
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("first-secret"));
    assert!(!debug.contains("second-secret"));
    assert!(!debug.contains("first.example"));
    assert!(!debug.contains("second.example"));
}

#[test]
fn catalog_chat_reasoning_variant_projects_wire_and_replays_opaque_content() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"chat_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "deep/chat#high",
            "providers": {
                "deep": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "DEEP_KEY",
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
            "DEEP_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("deep/chat#high"))
        .expect("selected Chat provider");
    let call = tool_call("call_1", "read");
    let mut request = ModelTurnRequest::new(
        "reasoning_chat_request",
        vec![
            ModelMessage::text(ModelRole::Developer, "instruction"),
            ModelMessage::assistant_tool_calls(vec![call.clone()]),
        ],
    );
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
        provider_name: "deep".to_string(),
        model_name: "chat".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec![call.tool_call_id.clone()],
        reasoning_content: "opaque-deepseek-state".to_string(),
    }];
    assert!(!format!("{request:?}").contains("opaque-deepseek-state"));
    assert!(
        serde_json::to_value(&request)
            .expect("serialize public request")
            .get("provider_reasoning_history")
            .is_none()
    );
    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("Chat reasoning completion");
    assert_eq!(response.status, ModelTurnStatus::Success);
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured Chat request"),
    )
    .expect("Chat payload JSON");
    assert_eq!(payload["model"], "chat");
    assert_eq!(payload["thinking"]["type"], "enabled");
    assert_eq!(payload["reasoning_effort"], "high");
    assert_eq!(payload["messages"][0]["role"], "system");
    assert_eq!(payload["messages"][1]["content"], "");
    assert_eq!(
        payload["messages"][1]["reasoning_content"],
        "opaque-deepseek-state"
    );
    assert!(payload.get("tools").is_none());
    assert!(payload.get("tool_choice").is_none());
}

#[test]
fn catalog_no_tool_request_accepts_system_and_developer_messages_before_wire_projection() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"catalog_no_tool_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "catalog/model",
            "providers": {
                "catalog": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "CATALOG_KEY",
                    "models": {
                        "model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "supports_developer_role": false
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
            "CATALOG_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("catalog/model"))
        .expect("selected catalog provider");
    let request = ModelTurnRequest::new(
        "catalog_no_tool_request",
        vec![
            ModelMessage::text(ModelRole::System, "system instruction"),
            ModelMessage::text(ModelRole::Developer, "developer instruction"),
            ModelMessage::text(ModelRole::User, "hello"),
        ],
    );
    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("catalog no-tool request");
    assert_eq!(response.status, ModelTurnStatus::Success);
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured catalog no-tool request"),
    )
    .expect("catalog no-tool payload JSON");
    let messages = payload["messages"]
        .as_array()
        .expect("catalog no-tool messages");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(messages[2]["role"], "user");
}

#[test]
fn env_provider_chat_projects_developer_role_to_system_without_a_selected_model() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"env_chat_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            "SINGULARITY_BASE_URL" => Some(base_url.clone()),
            "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot.provider().expect("env provider");
    let request = ModelTurnRequest::new(
        "env_developer_role_request",
        vec![
            ModelMessage::text(ModelRole::Developer, "instruction"),
            ModelMessage::text(ModelRole::User, "hello"),
        ],
    );
    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("env chat request");
    assert_eq!(response.status, ModelTurnStatus::Success);
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured env chat request"),
    )
    .expect("env chat payload JSON");
    let messages = payload["messages"].as_array().expect("env chat messages");
    // env 路径没有 per-model 声明：chat wire 必须用通用的 system role，
    // 不得发送 OpenAI 兼容端点普遍不接受的 developer（dashscope 实测 400）。
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
}

#[test]
fn catalog_enable_thinking_projects_dashscope_chat_fields_without_openai_thinking_object() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"dashscope_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "dashscope/deepseek-v4-flash-0731#max",
            "providers": {
                "dashscope": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "DASHSCOPE_KEY",
                    "models": {
                        "deepseek-v4-flash-0731": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 393216,
                            "thinking_wire_format": "enable_thinking",
                            "reasoning_variants": {
                                "max": {"enabled": true, "wire_effort": "max"}
                            },
                            "default_variant": "max"
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
            "DASHSCOPE_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("dashscope/deepseek-v4-flash-0731#max"))
        .expect("selected DashScope provider");
    provider
        .complete(
            &ModelTurnRequest::new(
                "dashscope_request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &singularity_core::CancellationToken::new(),
        )
        .expect("DashScope Chat completion");
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured DashScope request"),
    )
    .expect("DashScope payload JSON");
    assert_eq!(payload["model"], "deepseek-v4-flash-0731");
    assert_eq!(payload["enable_thinking"], true);
    assert_eq!(payload["reasoning_effort"], "max");
    assert!(payload.get("thinking").is_none());
}

#[test]
fn missing_provider_usage_remains_unknown() {
    // 响应缺 usage 时保留 usage_present=false，也不触发上限校验错误。
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"no_usage_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "opencode-go/deepseek-v4-flash",
            "providers": {
                "opencode-go": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "OPENCODE_KEY",
                    "models": {
                        "deepseek-v4-flash": {
                            "api_protocol": "chat",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000
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
            "OPENCODE_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("opencode-go/deepseek-v4-flash"))
        .expect("selected builtin provider");
    let response = provider
        .complete(
            &ModelTurnRequest::new(
                "missing_usage_request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &singularity_core::CancellationToken::new(),
        )
        .expect("Chat completion without usage");
    request_body
        .recv_timeout(Duration::from_secs(1))
        .expect("captured provider request");
    let usage = response.usage;
    assert!(!usage.usage_present, "missing usage must be marked absent");
    let validation = response.validation.expect("validation present");
    assert!(
        !validation
            .errors
            .iter()
            .any(|error| error == "response_output_tokens_exceed_provider_limit"),
        "upper-limit check must not fire on absent usage"
    );
}

#[test]
fn provider_usage_above_configured_output_limit_remains_accountable() {
    let base_url = single_response_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"usage_over_limit","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":9999,"total_tokens":10002}}"#,
    );
    let provider = test_provider(provider_test_config(base_url)).expect("provider");
    let response = provider
        .complete(
            &ModelTurnRequest::new(
                "usage_over_limit_request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &singularity_core::CancellationToken::new(),
        )
        .expect("usage above configured limit is a provider fact, not a schema failure");

    assert_eq!(response.usage.output_tokens, 9999);
    assert_eq!(response.usage.total_tokens, 10002);
    assert!(response.validation.expect("validation present").valid);
}

#[test]
fn catalog_unknown_context_remains_selectable_without_inventing_a_window() {
    let (base_url, request_body) = captured_request_server(
        "HTTP/1.1 200 OK",
        r#"{"id":"unknown_context_done","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
    );
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "unknown/model#max",
            "providers": {
                "unknown": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "UNKNOWN_KEY",
                    "models": {
                        "model": {
                            "api_protocol": "chat",
                            "max_output_tokens": 4096,
                            "reasoning_variants": {
                                "max": {"enabled": true, "wire_effort": "max"}
                            },
                            "default_variant": "max"
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
            "UNKNOWN_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("unknown/model#max"))
        .expect("selected unknown-context provider");
    provider
        .complete(
            &ModelTurnRequest::new(
                "unknown_context_request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &singularity_core::CancellationToken::new(),
        )
        .expect("unknown-context completion");
    let payload: serde_json::Value = serde_json::from_str(
        &request_body
            .recv_timeout(Duration::from_secs(1))
            .expect("captured unknown-context request"),
    )
    .expect("unknown-context payload JSON");
    assert_eq!(payload["model"], "model");
    assert_eq!(
        snapshot
            .provider()
            .expect("default provider")
            .protocol_contract()
            .max_context_tokens,
        None
    );
}

#[test]
fn catalog_rejects_explicit_output_limit_equal_to_context_window() {
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "invalid/model",
            "providers": {
                "invalid": {
                    "adapter": "openai_compatible",
                    "base_url": "https://provider.example/v1",
                    "api_key_env": "INVALID_KEY",
                    "models": {
                        "model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 4096,
                            "max_output_tokens": 4096
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write invalid catalog");
    let path = config_path.to_string_lossy().to_string();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "INVALID_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let error = snapshot
        .provider()
        .expect_err("invalid explicit limits must fail closed");
    assert!(error.message.contains("max_output_tokens"));
    assert!(error.message.contains("max_context_tokens"));
}

#[test]
fn catalog_responses_reasoning_variant_replays_standard_item_without_chat_field() {
    let (base_url, requests) = responses_provider_server(serde_json::json!({
        "id": "response_done",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "done"}]
        }]
    }));
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "longcat/responses#high",
            "providers": {
                "longcat": {
                    "adapter": "openai_compatible",
                    "base_url": base_url,
                    "api_key_env": "LONGCAT_KEY",
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
            "LONGCAT_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let provider = snapshot
        .provider_for_selector(Some("longcat/responses#high"))
        .expect("selected Responses provider");
    let unknown = snapshot
        .provider_for_selector(Some("longcat/responses#max"))
        .expect_err("unknown per-model variant must fail closed");
    assert_eq!(
        unknown.error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
    );
    let call = tool_call("call_1", "read");
    let mut request = ModelTurnRequest::new(
        "reasoning_responses_request",
        vec![ModelMessage::assistant_tool_calls(vec![call.clone()])],
    );
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Responses {
        provider_name: "longcat".to_string(),
        model_name: "responses".to_string(),
        reasoning_effort: "high".to_string(),
        tool_call_ids: vec![call.tool_call_id.clone()],
        items: vec![
            serde_json::json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "opaque-encrypted-state"
            }),
            serde_json::json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{}"
            }),
        ],
    }];
    let response = provider
        .complete(&request, &singularity_core::CancellationToken::new())
        .expect("Responses reasoning completion");
    assert_eq!(response.status, ModelTurnStatus::Success);
    let captured = requests
        .recv_timeout(Duration::from_secs(1))
        .expect("captured Responses requests");
    let payload: serde_json::Value =
        serde_json::from_str(&captured.last().expect("actual Responses request").1)
            .expect("Responses payload JSON");
    assert_eq!(payload["model"], "responses");
    assert_eq!(payload["reasoning"]["effort"], "high");
    assert_eq!(payload["include"][0], "reasoning.encrypted_content");
    assert!(payload.get("thinking").is_none());
    assert_eq!(payload["input"][0]["type"], "reasoning");
    assert_eq!(
        payload["input"][0]["encrypted_content"],
        "opaque-encrypted-state"
    );
    assert_eq!(payload["input"][1]["type"], "function_call");
}

#[test]
fn catalog_responses_reasoning_requires_explicit_wire_mapping() {
    let directory = tempdir().expect("catalog directory");
    let config_path = directory.path().join("models.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "default_model": "provider/responses#high",
            "providers": {
                "provider": {
                    "adapter": "openai_compatible",
                    "base_url": "https://provider.example/v1",
                    "api_key_env": "PROVIDER_KEY",
                    "models": {
                        "responses": {
                            "api_protocol": "responses",
                            "max_context_tokens": 1000000,
                            "max_output_tokens": 384000,
                            "reasoning_variants": {
                                "high": {"enabled": true}
                            },
                            "default_variant": "high"
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("write catalog");
    let path = config_path.to_string_lossy().into_owned();
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            ENV_MODELS_CONFIG => Some(path.clone()),
            "PROVIDER_KEY" => Some("sk-secret-value".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let error = snapshot
        .provider()
        .expect_err("Responses reasoning without a wire map must fail closed");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
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

/// 矩阵项 7：DisabledForToolCalls。`openai_chat_tool_history_finalization_rejects_reasoning_content`
/// 已精确覆盖 Chat 无当前 tool call 时返回 reasoning content 的失败关闭（请求无 tools）；
/// 本测试补充“请求含 tools 但响应仅 reasoning-only 无 tool call”的变体，覆盖新谓词
/// `disabled_mode_not_honored` 不依赖响应是否有 tool call 的路径。
// SINGULARITY_HOME 环境隔离：这两个端到端测试互相串行，其他测试不读该变量。
static USER_CONFIG_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn import_env_to_user_config_persists_config_auth_and_rejects_endpoint_change() {
    let _guard = USER_CONFIG_ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("home directory");
    unsafe { std::env::set_var("SINGULARITY_HOME", home.path()) };
    let env_path = home.path().join(".env");
    std::fs::write(
        &env_path,
        "SINGULARITY_BASE_URL=https://example.invalid/v1\nSINGULARITY_API_KEY=sk-secret-value\nSINGULARITY_MODEL=gpt-test\n",
    )
    .expect("dotenv file");

    let imported = singularity_model::import_env_to_user_config(Some(&env_path))
        .expect("import must persist the provider");
    assert!(std::path::Path::new(&imported.config_path).exists());
    assert!(std::path::Path::new(&imported.auth_path).exists());
    assert!(imported.config_path.ends_with("config.json"));
    assert!(imported.auth_path.contains("auth.v1-"));
    assert_eq!(imported.provider_name, "openai_compatible");

    // 相同 endpoint 重复导入幂等成功；endpoint 变更被拒绝。
    singularity_model::import_env_to_user_config(Some(&env_path))
        .expect("re-import with the same endpoint must succeed");
    std::fs::write(
        &env_path,
        "SINGULARITY_BASE_URL=https://changed.invalid/v1\nSINGULARITY_API_KEY=sk-secret-value\nSINGULARITY_MODEL=gpt-test\n",
    )
    .expect("changed dotenv file");
    let error = singularity_model::import_env_to_user_config(Some(&env_path))
        .expect_err("an endpoint change must be rejected");
    assert!(
        error.message.contains("endpoint"),
        "unexpected rejection message: {}",
        error.message
    );
    unsafe { std::env::remove_var("SINGULARITY_HOME") };
}

#[test]
fn read_user_model_catalog_serves_fresh_cache_and_explicit_models_without_network() {
    let _guard = USER_CONFIG_ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("home directory");
    unsafe { std::env::set_var("SINGULARITY_HOME", home.path()) };
    let env_path = home.path().join(".env");
    std::fs::write(
        &env_path,
        "SINGULARITY_BASE_URL=https://example.invalid/v1\nSINGULARITY_API_KEY=sk-secret-value\nSINGULARITY_MODEL=gpt-test\n",
    )
    .expect("dotenv file");
    singularity_model::import_env_to_user_config(Some(&env_path))
        .expect("import must persist the provider");

    // 预置新鲜缓存：endpoint_sha256 = sha256(normalized_endpoint_identity(base_url))。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs();
    std::fs::write(
        home.path().join("models-cache.json"),
        serde_json::json!({
            "schema_version": 1,
            "providers": {
                "openai_compatible": {
                    "endpoint_sha256": "3bf8b512d9020714a393483bfc3222d451ace7965f26bbf0c50286423b8ae0ce",
                    "fetched_at_unix_seconds": now,
                    "model_ids": ["gpt-discovered"]
                }
            }
        })
        .to_string(),
    )
    .expect("models cache file");

    let catalog = singularity_model::read_user_model_catalog(false, test_runtime_handle())
        .expect("catalog read must not require network on a fresh cache");
    assert_eq!(
        catalog.cache_status,
        singularity_model::ModelCacheStatus::Valid
    );
    let provider = catalog
        .providers
        .iter()
        .find(|provider| provider.provider_name == "openai_compatible")
        .expect("imported provider is present");
    assert_eq!(
        provider.discovery,
        singularity_model::ModelDiscoveryStatus::Fresh
    );
    let ids = provider
        .models
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.contains("gpt-test"), "explicit model id is listed");
    assert!(
        ids.contains("gpt-discovered"),
        "cached discovered id is listed"
    );
    let gpt_test = provider
        .models
        .iter()
        .find(|entry| entry.id == "gpt-test")
        .expect("explicit model entry");
    assert!(gpt_test.explicit);
    assert!(!gpt_test.discovered);

    // 再次读取仍命中同一新鲜缓存。
    let again = singularity_model::read_user_model_catalog(false, test_runtime_handle())
        .expect("second catalog read");
    let again_provider = again
        .providers
        .iter()
        .find(|provider| provider.provider_name == "openai_compatible")
        .expect("provider still present");
    assert_eq!(
        again_provider.discovery,
        singularity_model::ModelDiscoveryStatus::Fresh
    );
    unsafe { std::env::remove_var("SINGULARITY_HOME") };
}
