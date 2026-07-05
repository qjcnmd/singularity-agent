use singularity_tools::{
    ToolCallEnvelope, ToolObservation, ToolObservationVisibility, ToolRegistry, ToolResult,
    ToolSpec,
};

#[test]
fn tool_observation_model_payload_hides_internal_metadata() {
    let observation =
        ToolObservation::summary("call_1", "read_file", true, "safe preview", "digest_1")
            .with_internal_metadata(
                "policy_1",
                "grant_1",
                serde_json::json!({"raw_arguments": {"path": ".env"}}),
            );

    let payload = observation.to_model_payload();

    assert_eq!(payload["tool_call_id"], "call_1");
    assert_eq!(payload["content_preview"], "safe preview");
    assert!(payload.get("policy_decision_id").is_none());
    assert!(payload.get("approval_grant_id").is_none());
    assert!(payload.get("metadata").is_none());
    assert!(
        !serde_json::to_string(&payload)
            .unwrap()
            .contains("raw_arguments")
    );
}

#[test]
fn registry_rejects_duplicate_tools() {
    let mut registry = ToolRegistry::default();
    let spec = ToolSpec::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    );

    registry
        .register(spec.clone())
        .expect("first registration succeeds");

    assert!(registry.register(spec).is_err());

    let envelope =
        ToolCallEnvelope::new("run_1", "session_1", "task_1", "call_1", "read_file", "{}");
    let result = ToolResult::success(serde_json::json!({"ok": true}));
    let observation =
        ToolObservation::from_result(&envelope, &result, ToolObservationVisibility::Summary);
    assert_eq!(observation.tool_name, "read_file");
}
