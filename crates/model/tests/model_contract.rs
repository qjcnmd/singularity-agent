use singularity_model::{
    ModelMessage, ModelRole, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus,
};

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
