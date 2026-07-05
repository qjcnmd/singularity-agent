use singularity_agent::{
    AgentHostStatus, AgentLoopStatusBridge, ContextAssemblyBoundary, PlannerStateBoundary,
    PythonSidecarClient, PythonSidecarConfig, PythonSidecarRunResult, SidecarRunEvent,
    sidecar_trace_summary,
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
