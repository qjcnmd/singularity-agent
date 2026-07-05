use singularity_agent::{
    AgentHostStatus, AgentLoopStatusBridge, ContextAssemblyBoundary,
    ContextSummaryEnvelopeBoundary, FinalizationMappingBoundary, PlannerStateBoundary,
    PythonSidecarClient, PythonSidecarConfig, PythonSidecarRunResult, SidecarRunEvent,
    ToolCallRepairBoundary, sidecar_trace_summary,
};

#[test]
fn agent_boundary_reports_not_migrated_without_claiming_completion() {
    let bridge = AgentLoopStatusBridge::not_migrated();

    assert_eq!(bridge.status, AgentHostStatus::NotMigrated);
    assert!(!bridge.completed);
    assert_eq!(bridge.status.as_str(), "not_migrated");
    assert!(bridge.final_answer.is_none());
}

#[test]
fn sidecar_result_maps_agent_loop_status_without_raw_payloads() {
    let result = PythonSidecarRunResult {
        run_id: "run_1".to_string(),
        session_id: "session_1".to_string(),
        task_id: "task_1".to_string(),
        status: "completed".to_string(),
        final_answer: Some("done".to_string()),
        trace_path: Some("run_1".to_string()),
        events: vec![SidecarRunEvent {
            event_id: "event_1".to_string(),
            event_type: "lifecycle.run.started".to_string(),
            summary: "started".to_string(),
            component: "kernel".to_string(),
            severity: "info".to_string(),
            sequence: 0,
        }],
    };

    let bridge = AgentLoopStatusBridge::from_sidecar(result);
    let summary = sidecar_trace_summary(&bridge);

    assert_eq!(bridge.status, AgentHostStatus::Completed);
    assert!(bridge.completed);
    assert_eq!(bridge.final_answer.as_deref(), Some("done"));
    assert_eq!(summary["component"], "python_sidecar");
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["trace_path"], "run_1");
    assert!(summary.get("raw_prompt").is_none());
}

#[test]
fn sidecar_status_mapping_preserves_blocked_and_cancelled() {
    assert_eq!(AgentHostStatus::from("blocked"), AgentHostStatus::Blocked);
    assert_eq!(
        AgentHostStatus::from("cancelled"),
        AgentHostStatus::Cancelled
    );
    assert_eq!(
        AgentHostStatus::from("max_turns_exceeded"),
        AgentHostStatus::Failed
    );
}

#[test]
fn sidecar_startup_failure_is_reported_without_fallback() {
    let config = PythonSidecarConfig {
        python_bin: "definitely_missing_python_sidecar_binary".to_string(),
        module: "singularity.agent_host.sidecar".to_string(),
        project_root: std::env::current_dir().expect("cwd"),
        python_path: None,
        env: Vec::new(),
    };

    let error = PythonSidecarClient::spawn(&config).expect_err("sidecar spawn should fail");

    assert!(error.contains("failed to start Python sidecar"));
}

#[test]
fn planner_state_boundary_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let planner: PlannerStateBoundary =
        serde_json::from_value(fixture["planner_state"].clone()).expect("planner boundary");

    assert_eq!(planner.task_id, "task_1");
    assert_eq!(planner.current_phase, "running_verification");
    assert_eq!(planner.status, "repairing_failures");
    assert_eq!(planner.evidence_refs, vec!["obs_1"]);

    assert_eq!(
        serde_json::from_value::<PlannerStateBoundary>(
            serde_json::to_value(&planner).expect("serialize planner")
        )
        .expect("deserialize planner"),
        planner
    );
}

#[test]
fn context_assembly_boundary_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let context: ContextAssemblyBoundary =
        serde_json::from_value(fixture["context_bundle"].clone()).expect("context boundary");

    assert_eq!(context.bundle_id, "bundle_1");
    assert_eq!(context.phase_id, "running_verification");
    assert_eq!(context.included_item_ids, vec!["item_goal", "item_plan"]);
    assert_eq!(context.excluded_item_ids, vec!["item_raw_tool"]);
    assert_eq!(context.budget["model_context_window"], 128000);
    assert_eq!(context.budget["message_tokens"], 62);
    assert_eq!(context.render_policy["include_raw_tool_outputs"], false);
    assert_eq!(context.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<ContextAssemblyBoundary>(
            serde_json::to_value(&context).expect("serialize context")
        )
        .expect("deserialize context"),
        context
    );
}

#[test]
fn context_summary_envelope_boundary_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let summary: ContextSummaryEnvelopeBoundary =
        serde_json::from_value(fixture["context_summary_envelope"].clone())
            .expect("context summary boundary");

    assert_eq!(summary.version, 1);
    assert_eq!(summary.summary_id, "summary_1");
    assert_eq!(summary.source_item_ids, vec!["item_raw_tool"]);
    assert_eq!(summary.summary_payload["verification_status"], "passed");
    assert_eq!(
        summary.summary_payload["omitted_item_ids"],
        serde_json::json!(["item_raw_tool"])
    );
    assert_eq!(summary.cache_attribution["source"], "component_inferred");
    assert_eq!(summary.metadata["source"], "python_oracle");
    assert!(summary.rendered_summary.contains("verification=passed"));

    assert_eq!(
        serde_json::from_value::<ContextSummaryEnvelopeBoundary>(
            serde_json::to_value(&summary).expect("serialize context summary")
        )
        .expect("deserialize context summary"),
        summary
    );
}

#[test]
fn tool_call_repair_boundary_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let repair: ToolCallRepairBoundary =
        serde_json::from_value(fixture["tool_call_repair_boundary"].clone())
            .expect("tool repair boundary");

    assert_eq!(repair.repair_id, "tool_repair_1");
    assert_eq!(repair.failed_tool_call_id, "call_failed_1");
    assert_eq!(repair.failure_kind, "tool_executor_failed");
    assert_eq!(repair.next_action, "repair_then_verify");
    assert_eq!(repair.failed_result["ok"], false);
    assert_eq!(
        repair.recovery_report["succeeded_but_not_appended_call_ids"],
        serde_json::json!(["call_failed_1"])
    );
    assert_eq!(
        repair.repair_contract["allowed_tool_names"],
        serde_json::json!(["apply_patch", "read_file", "run_verification"])
    );
    assert_eq!(
        repair.repair_contract["verification_contract"]["contract_id"],
        "verification_contract_1"
    );
    assert_eq!(repair.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<ToolCallRepairBoundary>(
            serde_json::to_value(&repair).expect("serialize tool repair")
        )
        .expect("deserialize tool repair"),
        repair
    );
}

#[test]
fn finalization_mapping_boundary_round_trips_python_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/rust_parity/python_oracle.json"
    ))
    .expect("parse python oracle fixture");

    let mapping: FinalizationMappingBoundary =
        serde_json::from_value(fixture["finalization_mapping_boundary"].clone())
            .expect("finalization mapping boundary");

    assert_eq!(mapping.mapping_id, "finalization_mapping_1");
    assert_eq!(mapping.phase_id, "finalizing");
    assert_eq!(mapping.agent_loop_status, "completed");
    assert_eq!(mapping.run_status, "completed");
    assert_eq!(mapping.final_report_status, "completed");
    assert_eq!(mapping.completion_status, "completed");
    assert_eq!(
        mapping.final_report["verification_summary"]["status"],
        "ready"
    );
    assert_eq!(
        mapping.completion_assessment["unmet"],
        serde_json::json!([])
    );
    assert_eq!(mapping.contract_satisfaction["satisfied"], true);
    assert!(mapping.final_answer.contains("verification: ready"));
    assert_eq!(mapping.metadata["source"], "python_oracle");

    assert_eq!(
        serde_json::from_value::<FinalizationMappingBoundary>(
            serde_json::to_value(&mapping).expect("serialize finalization mapping")
        )
        .expect("deserialize finalization mapping"),
        mapping
    );
}
