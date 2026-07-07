use schemars::schema_for;
use singularity_model::{
    ModelBlockerKind, ModelCapabilities, ModelError, ModelErrorCategory, ModelErrorKind,
    ModelMessage, ModelProviderConfig, ModelProviderStatus, ModelRole, ModelToolCall,
    ModelToolParseStatus, ModelToolSchema, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus,
    ModelUsage, ProviderStreamEvent, ProviderStreamEventType, ToolChoiceMode, ToolChoicePolicy,
    classify_model_error, provider_config_from_env, retry_decision, validate_model_request,
    validate_model_response, validate_model_turn_response, validate_provider_config,
    validate_stream_events,
};

fn stream_event(event_type: ProviderStreamEventType) -> ProviderStreamEvent {
    ProviderStreamEvent {
        event_type,
        text_delta: None,
        tool_call_id: None,
        tool_name: None,
        arguments_delta: None,
        usage_delta: None,
        error: None,
        metadata: serde_json::json!({}),
    }
}

fn tool_call(id: &str, name: &str) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: id.to_string(),
        tool_name: name.to_string(),
        arguments: serde_json::json!({"path": "README.md"}),
        raw_arguments: r#"{"path":"README.md"}"#.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
        provider_metadata: serde_json::json!({}),
    }
}

#[test]
fn model_turn_request_uses_python_style_embedded_fields() {
    let request = ModelTurnRequest::new(
        "request_1",
        "run_1",
        "session_1",
        "task_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let value = serde_json::to_value(&request).expect("serialize model request");

    assert_eq!(value["request_id"], "request_1");
    assert_eq!(value["messages"][0]["role"], "user");

    let response = ModelTurnResponse::completed("request_1", "response_1", "done");
    assert_eq!(response.status, ModelTurnStatus::Success);
}

#[test]
fn model_turn_schema_carries_python_oracle_boundary_fields() {
    let request = ModelTurnRequest::new(
        "request_1",
        "run_1",
        "session_1",
        "task_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    );
    let value = serde_json::to_value(&request).expect("serialize model request");

    assert_eq!(value["phase_id"], "model");
    assert_eq!(value["action_id"], "request_1");
    assert_eq!(value["tools"], serde_json::json!([]));
    assert_eq!(value["tool_choice"]["mode"], "auto");
    assert_eq!(value["budget"]["max_retries"], 2);
    assert!(value["model_preferences"]["stream"].as_bool().is_some());
    assert!(value["context_metadata"].is_object());
    assert!(value["policy_metadata"].is_object());
    assert!(value["trace_metadata"].is_object());

    let response = ModelTurnResponse::completed("request_1", "response_1", "done");
    let response_value = serde_json::to_value(&response).expect("serialize model response");

    assert_eq!(response_value["assistant_message"]["role"], "assistant");
    assert_eq!(response_value["tool_calls"], serde_json::json!([]));
    assert_eq!(response_value["usage"]["total_tokens"], 0);
    assert!(response_value["trace_event_ids"].is_array());
}

#[test]
fn model_turn_matches_python_oracle_key_fields() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");
    let request = ModelTurnRequest::new(
        "model_req_1",
        "run_1",
        "session_1",
        "task_1",
        vec![ModelMessage::text(ModelRole::User, "hello")],
    )
    .with_phase_action("model", "action_1");
    let response = ModelTurnResponse::completed("model_req_1", "response_1", "done");
    let request_value = serde_json::to_value(request).expect("serialize model request");
    let response_value = serde_json::to_value(response).expect("serialize model response");

    for field in [
        "request_id",
        "run_id",
        "session_id",
        "task_id",
        "phase_id",
        "action_id",
        "purpose",
        "tools",
        "tool_choice",
        "model_preferences",
        "budget",
        "context_metadata",
        "policy_metadata",
        "trace_metadata",
    ] {
        assert_eq!(
            request_value[field], fixture["model_turn_request"][field],
            "request field {field}"
        );
    }
    assert_eq!(
        request_value["messages"][0]["role"],
        fixture["model_turn_request"]["messages"][0]["role"]
    );
    assert_eq!(
        request_value["messages"][0]["content"][0]["text"],
        fixture["model_turn_request"]["messages"][0]["content"][0]["text"]
    );

    for field in [
        "request_id",
        "response_id",
        "status",
        "tool_calls",
        "usage",
        "finish_reason",
        "validation",
        "error",
        "provider_name",
        "model_name",
        "latency_ms",
        "trace_event_ids",
        "raw_response_ref",
        "metadata",
    ] {
        assert_eq!(
            response_value[field], fixture["model_turn_response"][field],
            "response field {field}"
        );
    }
    assert_eq!(
        response_value["assistant_message"]["content"][0]["text"],
        fixture["model_turn_response"]["assistant_message"]["content"][0]["text"]
    );
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
fn provider_config_loads_presence_from_env_without_secret_values() {
    let config = provider_config_from_env(|name| match name {
        "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
        "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
        "SINGULARITY_BASE_URL" => Some("https://provider.example/v1".to_string()),
        "SINGULARITY_API_KEY" => Some("sk-secret-value".to_string()),
        _ => None,
    });
    let status = ModelProviderStatus::from_config(&config);
    let serialized = serde_json::to_string(&status).expect("serialize provider status");

    assert_eq!(config.provider_name.as_deref(), Some("openai_compatible"));
    assert_eq!(config.model_name.as_deref(), Some("gpt-test"));
    assert!(config.base_url_present);
    assert!(config.api_key_present);
    assert!(status.ready);
    assert_eq!(status.api_key_status, "present(redacted)");
    assert_eq!(status.base_url_status, "present(redacted)");
    assert!(!serialized.contains("sk-secret-value"));
    assert!(!serialized.contains("provider.example"));
}

#[test]
fn provider_status_reports_required_env_missing_blocker() {
    let status = ModelProviderStatus::from_config(&ModelProviderConfig {
        provider_name: Some("openai_compatible".to_string()),
        model_name: None,
        base_url_present: true,
        api_key_present: false,
    });

    assert!(!status.ready);
    assert_eq!(status.blocker, Some(ModelBlockerKind::RequiredEnvMissing));
    assert_eq!(
        status.blocker.as_ref().unwrap().as_str(),
        "required env missing"
    );
}

#[test]
fn model_errors_classify_provider_failures_without_transport_calls() {
    let auth = ModelError::new(ModelErrorKind::AuthError, "Provider returned HTTP 401.")
        .with_provider("openai_compatible")
        .with_model("gpt-test");

    assert_eq!(
        classify_model_error(&auth),
        ModelErrorCategory::Authentication
    );
    assert!(!auth.retryable);

    let permission_denied = ModelError::new(
        ModelErrorKind::NetworkError,
        "[WinError 10013] socket access denied",
    )
    .retryable(false);

    assert_eq!(
        permission_denied.category(),
        ModelErrorCategory::SandboxPermission
    );

    let model_missing = ModelError::new(
        ModelErrorKind::InvalidRequest,
        "model gpt-missing does not exist",
    );

    assert_eq!(
        model_missing.category(),
        ModelErrorCategory::ModelConfiguration
    );
}

#[test]
fn retry_decision_is_bounded_to_retryable_provider_errors() {
    let rate_limited = ModelError::new(ModelErrorKind::RateLimited, "Provider returned HTTP 429.");
    let validation = ModelError::new(ModelErrorKind::JsonSchemaViolation, "schema mismatch");

    let retry = retry_decision(&rate_limited, 0, 2);
    let exhausted = retry_decision(&rate_limited, 2, 2);
    let non_retryable = retry_decision(&validation, 0, 2);

    assert!(retry.retry);
    assert_eq!(retry.next_attempt, Some(1));
    assert_eq!(retry.reason.as_deref(), Some("retryable_model_error"));
    assert!(!exhausted.retry);
    assert_eq!(exhausted.reason.as_deref(), Some("retry_budget_exhausted"));
    assert!(!non_retryable.retry);
    assert_eq!(
        non_retryable.reason.as_deref(),
        Some("non_retryable_model_error")
    );
}

#[test]
fn request_and_response_validation_helpers_reject_empty_or_mismatched_envelopes() {
    let mut request = ModelTurnRequest::new("request_1", "run_1", "session_1", "task_1", vec![]);
    request.model_preferences.provider_name = Some("openai_compatible".to_string());
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
    let failure = ModelError::new(ModelErrorKind::RateLimited, "Provider returned HTTP 429.")
        .retryable(true)
        .with_provider("openai_compatible")
        .with_model("gpt-test");

    let value = serde_json::to_value(&failure).expect("serialize provider failure");

    assert_eq!(value["kind"], "rate_limited");
    assert_eq!(value["retryable"], true);
    assert_eq!(value["provider_name"], "openai_compatible");
    assert_eq!(value["model_name"], "gpt-test");
    assert!(value["raw_error_ref"].is_null());
    assert!(!value.to_string().contains("sk-"));
}

#[test]
fn streaming_events_validate_minimal_envelope_schema() {
    let events = vec![
        ProviderStreamEvent {
            text_delta: Some("he".to_string()),
            ..stream_event(ProviderStreamEventType::TextDelta)
        },
        ProviderStreamEvent {
            text_delta: Some("llo".to_string()),
            ..stream_event(ProviderStreamEventType::TextDelta)
        },
        ProviderStreamEvent {
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("builtin.read_file".to_string()),
            arguments_delta: Some(r#"{"path":"#.to_string()),
            ..stream_event(ProviderStreamEventType::ToolCallDelta)
        },
        ProviderStreamEvent {
            tool_call_id: Some("call_1".to_string()),
            ..stream_event(ProviderStreamEventType::ToolCallCompleted)
        },
        ProviderStreamEvent {
            usage_delta: Some(Default::default()),
            ..stream_event(ProviderStreamEventType::UsageDelta)
        },
        stream_event(ProviderStreamEventType::ResponseCompleted),
    ];

    assert!(validate_stream_events(&events).valid);

    let value = serde_json::to_value(&events[0]).expect("serialize stream event");
    assert_eq!(value["type"], "text_delta");
    assert_eq!(value["text_delta"], "he");
}

#[test]
fn streaming_events_reject_bad_envelopes_and_events_after_completion() {
    let events = vec![
        ProviderStreamEvent {
            tool_name: Some("builtin.read_file".to_string()),
            ..stream_event(ProviderStreamEventType::ToolCallDelta)
        },
        stream_event(ProviderStreamEventType::ResponseCompleted),
        ProviderStreamEvent {
            text_delta: Some("late".to_string()),
            ..stream_event(ProviderStreamEventType::TextDelta)
        },
    ];

    let result = validate_stream_events(&events);

    assert!(!result.valid);
    assert_eq!(
        result.errors,
        vec![
            "stream_event[0].tool_call_id_required",
            "stream_event[2].event_after_response_completed",
        ]
    );
}

#[test]
fn streaming_events_require_delta_before_tool_completion() {
    let result = validate_stream_events(&[ProviderStreamEvent {
        tool_call_id: Some("call_1".to_string()),
        ..stream_event(ProviderStreamEventType::ToolCallCompleted)
    }]);

    assert!(!result.valid);
    assert_eq!(
        result.errors,
        vec!["stream_event[0].tool_call_delta_required"]
    );
}

#[test]
fn model_response_validation_enforces_tool_choice_and_provider_capabilities() {
    let call = tool_call("call_1", "builtin.read_file");
    let none_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call.clone()],
        &ToolChoicePolicy {
            mode: ToolChoiceMode::None,
            ..Default::default()
        },
        &["builtin.read_file".to_string()],
        None,
    );

    assert!(!none_result.valid);
    assert_eq!(none_result.errors, vec!["tool_choice_none"]);

    let duplicate_result = validate_model_response(
        Some(&ModelMessage::text(ModelRole::Assistant, "")),
        &[call.clone(), call],
        &ToolChoicePolicy::default(),
        &["builtin.read_file".to_string()],
        Some(&ModelCapabilities::default()),
    );

    assert_eq!(
        duplicate_result.errors,
        vec![
            "duplicate_tool_call_id",
            "provider_does_not_support_parallel_tool_calls"
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
        capability_tags: vec!["read".to_string()],
        risk_tags: vec!["workspace_read".to_string()],
        metadata: serde_json::json!({}),
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
    let stream = ProviderStreamEvent {
        text_delta: Some("hello".to_string()),
        ..stream_event(ProviderStreamEventType::TextDelta)
    };

    let restored_schema: ModelToolSchema =
        serde_json::from_value(serde_json::to_value(&tool_schema).unwrap()).unwrap();
    let restored_config: ModelProviderConfig =
        serde_json::from_value(serde_json::to_value(&provider_config).unwrap()).unwrap();
    let restored_error: ModelError =
        serde_json::from_value(serde_json::to_value(&model_error).unwrap()).unwrap();
    let restored_stream: ProviderStreamEvent =
        serde_json::from_value(serde_json::to_value(&stream).unwrap()).unwrap();

    assert_eq!(restored_schema, tool_schema);
    assert_eq!(restored_config, provider_config);
    assert_eq!(restored_error, model_error);
    assert_eq!(restored_stream, stream);
    assert_eq!(schema_title::<ModelToolSchema>(), "ModelToolSchema");
    assert_eq!(schema_title::<ModelToolCall>(), "ModelToolCall");
    assert_eq!(schema_title::<ModelCapabilities>(), "ModelCapabilities");
    assert_eq!(schema_title::<ModelProviderConfig>(), "ModelProviderConfig");
    assert_eq!(schema_title::<ModelUsage>(), "ModelUsage");
    assert_eq!(schema_title::<ProviderStreamEvent>(), "ProviderStreamEvent");
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
