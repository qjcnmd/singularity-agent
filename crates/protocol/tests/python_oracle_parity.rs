use schemars::schema_for;
use serde_json::Value;
use singularity_protocol::{TraceEvent, TurnStartParams};
use singularity_tools::ToolResult;

#[test]
fn python_oracle_fixture_has_expected_safe_tool_result_shape() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");
    let payload = &fixture["tool_result_payload"];
    let output = &fixture["tool_output"];
    let reference_output = &fixture["reference_tool_output"];

    assert_eq!(payload["tool_call_id"], "call_1");
    assert_eq!(
        output
            .as_object()
            .expect("tool output object")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["content", "error_code", "metadata", "ok", "truncated"]
    );
    assert!(payload.get("policy_decision_id").is_none());
    assert!(payload.get("approval_grant_id").is_none());
    assert!(payload.get("metadata").is_none());
    assert!(output.get("tool_call_id").is_none());
    assert!(output.get("tool_name").is_none());
    assert!(output.get("status").is_none());
    assert!(reference_output["content"]["preview"].is_null());
    assert_eq!(reference_output["content"]["artifact_ref"], "artifact_1");
    assert_eq!(reference_output["truncated"], true);
    assert!(
        !serde_json::to_string(payload)
            .unwrap()
            .contains("raw_arguments")
    );
}

#[test]
fn rust_protocol_and_tool_result_schemas_are_generated() {
    let turn_schema = schema_for!(TurnStartParams);
    let trace_schema = schema_for!(TraceEvent);
    let tool_result_schema = schema_for!(ToolResult);

    assert_eq!(
        turn_schema.schema.metadata.unwrap().title.unwrap(),
        "TurnStartParams"
    );
    assert_eq!(
        trace_schema.schema.metadata.unwrap().title.unwrap(),
        "TraceEvent"
    );
    assert_eq!(
        tool_result_schema.schema.metadata.unwrap().title.unwrap(),
        "ToolResult"
    );
}

#[test]
fn trace_event_schema_carries_python_oracle_boundary_fields() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");
    let rust_trace = TraceEvent {
        event_id: "event_1".to_string(),
        event_type: "tool_protocol.call_completed".to_string(),
        run_id: "run_1".to_string(),
        session_id: "session_1".to_string(),
        task_id: Some("task_1".to_string()),
        phase_id: Some("phase_1".to_string()),
        action_id: Some("action_1".to_string()),
        parent_event_id: None,
        timestamp: Some("2026-01-01T00:00:00+00:00".to_string()),
        monotonic_ms: Some(1),
        component: "tool_protocol".to_string(),
        severity: "info".to_string(),
        summary: "Tool completed.".to_string(),
        payload: serde_json::json!({"tool_call_id": "call_1"}),
        artifact_refs: Vec::new(),
        policy_decision_id: None,
        approval_grant_id: None,
        sandbox_id: None,
        command_id: None,
        transaction_id: None,
        verification_id: None,
        span_id: None,
        redaction_applied: true,
        payload_hash: String::new(),
    };

    assert_eq!(
        serde_json::to_value(rust_trace).expect("serialize rust trace"),
        fixture["trace_event"]
    );
}
