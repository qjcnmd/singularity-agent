use singularity_agent::{
    AgentContextItem, AgentContextItemPriority, AgentHostStatus, AgentLoopStatusBridge,
    CompletionGateInput, ContextAssemblyBoundary, ContextSummaryEnvelopeBoundary,
    EvaluationDiagnostics, EvaluationRunReport, FinalizationMappingBoundary, NativeAgentLoop,
    NativeAgentLoopCapability, NativeAgentLoopInput, NativeAgentLoopStep, PlannerNextAction,
    PlannerStateBoundary, PythonSidecarClient, PythonSidecarConfig, PythonSidecarRunResult,
    RepairNextAction, SidecarRunEvent, ToolCallRepairBoundary, assemble_context_items,
    completion_gate_allows_final, final_mapping_from_status, planner_next_action,
    repair_next_action, sidecar_trace_summary,
};
use std::time::Duration;

const TEST_SIDECAR_RESPONSE_TIMEOUT_MS: u64 = 100;

#[test]
fn agent_boundary_reports_not_migrated_without_claiming_completion() {
    let bridge = AgentLoopStatusBridge::not_migrated();

    assert_eq!(bridge.status, AgentHostStatus::NotMigrated);
    assert!(!bridge.completed);
    assert_eq!(bridge.status.as_str(), "not_migrated");
    assert!(bridge.final_answer.is_none());
}

#[test]
fn native_agent_loop_capability_is_explicitly_not_available() {
    let capability = NativeAgentLoopCapability::current();
    let bridge = capability.status_bridge();

    assert!(!capability.available);
    assert_eq!(capability.status, AgentHostStatus::NotMigrated);
    assert!(capability.reason.contains("not migrated"));
    assert!(
        capability
            .missing_boundaries
            .contains(&"planner_step".to_string())
    );
    assert!(
        capability
            .missing_boundaries
            .contains(&"finalizer_runtime".to_string())
    );
    assert_eq!(bridge.status, AgentHostStatus::NotMigrated);
    assert!(!bridge.completed);
}

#[test]
fn native_agent_loop_plan_lists_real_integration_steps_without_running_them() {
    let plan = NativeAgentLoop::integration_plan();

    assert_eq!(
        plan.steps,
        vec![
            NativeAgentLoopStep::LoadTurn,
            NativeAgentLoopStep::AssembleContext,
            NativeAgentLoopStep::CallModel,
            NativeAgentLoopStep::AdmitToolCalls,
            NativeAgentLoopStep::ExecuteApprovedTools,
            NativeAgentLoopStep::AppendObservations,
            NativeAgentLoopStep::RepairOnFailure,
            NativeAgentLoopStep::FinalizeReport,
            NativeAgentLoopStep::PersistItemsTraceArtifacts,
            NativeAgentLoopStep::HandleInterrupt,
        ]
    );
    assert!(
        plan.merge_requirements
            .contains(&"model_provider_adapter".to_string())
    );
    assert!(
        plan.merge_requirements
            .contains(&"tool_execution_runtime".to_string())
    );
}

#[test]
fn native_agent_loop_run_is_blocked_until_capability_is_available() {
    let input = NativeAgentLoopInput {
        thread_id: "thread_1".to_string(),
        turn_id: "turn_1".to_string(),
    };
    let result = NativeAgentLoop::run(&input, &NativeAgentLoopCapability::current());

    assert_eq!(result.status, AgentHostStatus::NotMigrated);
    assert!(!result.completed);
    assert!(result.final_answer.is_none());
    assert!(result.error.as_deref().unwrap().contains("not migrated"));
}

#[test]
fn evaluation_report_contract_keeps_gate_fields_separate_from_diagnostics() {
    let report = EvaluationRunReport {
        evaluation_passed: false,
        agent_completed: true,
        tests_passed: true,
        public_verification_passed: true,
        hidden_verification_passed: false,
        local_process_fallback_count: 0,
        diagnostics: EvaluationDiagnostics {
            base_verification_passed: Some(false),
            sandbox_required: true,
            notes: vec!["diagnostic-only timing note".to_string()],
        },
    };

    let value = serde_json::to_value(&report).expect("serialize evaluation report");

    assert_eq!(value["evaluation_passed"], false);
    assert_eq!(value["agent_completed"], true);
    assert_eq!(value["tests_passed"], true);
    assert_eq!(value["public_verification_passed"], true);
    assert_eq!(value["hidden_verification_passed"], false);
    assert_eq!(value["local_process_fallback_count"], 0);
    assert_eq!(value["diagnostics"]["base_verification_passed"], false);
    assert_eq!(value["diagnostics"]["sandbox_required"], true);
    assert!(value["diagnostics"].get("evaluation_passed").is_none());
    assert!(value.get("base_verification_passed").is_none());

    let round_trip: EvaluationRunReport =
        serde_json::from_value(value).expect("deserialize evaluation report");
    assert_eq!(round_trip, report);
}

#[test]
fn context_assembly_keeps_user_turn_and_safe_tool_observations_with_budget() {
    let items = vec![
        AgentContextItem {
            item_id: "tool_raw".to_string(),
            role: "tool".to_string(),
            content: "raw".to_string(),
            priority: AgentContextItemPriority::Evidence,
            token_count: 3,
            safe_for_model: false,
            evaluator_only: false,
            digest: "digest_raw".to_string(),
        },
        AgentContextItem {
            item_id: "user_1".to_string(),
            role: "user".to_string(),
            content: "fix tests".to_string(),
            priority: AgentContextItemPriority::CurrentTurn,
            token_count: 6,
            safe_for_model: true,
            evaluator_only: false,
            digest: "digest_user".to_string(),
        },
        AgentContextItem {
            item_id: "eval_1".to_string(),
            role: "system".to_string(),
            content: "hidden scorer".to_string(),
            priority: AgentContextItemPriority::System,
            token_count: 4,
            safe_for_model: true,
            evaluator_only: true,
            digest: "digest_eval".to_string(),
        },
        AgentContextItem {
            item_id: "tool_safe".to_string(),
            role: "tool".to_string(),
            content: "safe preview".to_string(),
            priority: AgentContextItemPriority::Evidence,
            token_count: 5,
            safe_for_model: true,
            evaluator_only: false,
            digest: "digest_tool".to_string(),
        },
    ];

    let context = assemble_context_items(&items, 11);

    assert_eq!(context.included_item_ids, vec!["user_1", "tool_safe"]);
    assert_eq!(context.excluded_item_ids, vec!["tool_raw", "eval_1"]);
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0]["role"], "user");
    assert_eq!(context.messages[1]["role"], "tool");
    assert_eq!(context.budget["message_tokens"], 11);
    assert!(context.bundle_digest.contains("digest_user"));
    assert!(context.bundle_digest.contains("digest_tool"));
}

#[test]
fn planner_repair_completion_and_final_mapping_are_deterministic() {
    let pending_approval = PlannerStateBoundary {
        task_id: "task_1".to_string(),
        current_phase: "running_verification".to_string(),
        status: "running".to_string(),
        current_plan: Vec::new(),
        completion_criteria: serde_json::json!({}),
        open_actions: vec![serde_json::json!({"kind": "approval", "status": "pending"})],
        blocked_actions: Vec::new(),
        risk_escalations: Vec::new(),
        evidence_refs: Vec::new(),
    };
    let pending_tool = PlannerStateBoundary {
        open_actions: vec![serde_json::json!({"kind": "tool", "status": "pending"})],
        ..pending_approval.clone()
    };
    let repair = ToolCallRepairBoundary {
        repair_id: "repair_1".to_string(),
        run_id: "run_1".to_string(),
        session_id: "session_1".to_string(),
        task_id: "task_1".to_string(),
        phase_id: "repairing_failures".to_string(),
        failed_tool_call_id: "call_1".to_string(),
        failure_kind: "tool_executor_failed".to_string(),
        next_action: "repair_then_verify".to_string(),
        failed_result: serde_json::json!({"ok": false}),
        recovery_report: serde_json::json!({}),
        repair_contract: serde_json::json!({}),
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        metadata: serde_json::json!({}),
    };

    assert_eq!(
        planner_next_action(&pending_approval),
        PlannerNextAction::ResumePendingApproval
    );
    assert_eq!(
        planner_next_action(&pending_tool),
        PlannerNextAction::ExecutePendingTool
    );
    assert_eq!(
        repair_next_action(&repair),
        RepairNextAction::RepairThenVerify
    );
    assert!(!completion_gate_allows_final(&CompletionGateInput {
        verification_passed: false,
        unresolved_failures: Vec::new(),
        interrupted: false,
    }));

    let mapping = final_mapping_from_status(
        "mapping_1",
        "run_1",
        "session_1",
        "task_1",
        AgentHostStatus::Completed,
        "done",
    );

    assert_eq!(mapping.run_status, "completed");
    assert_eq!(mapping.final_report_status, "completed");
    assert_eq!(mapping.completion_status, "completed");
    assert_eq!(mapping.final_answer, "done");
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
fn sidecar_result_ignores_raw_payload_and_metadata_fields() {
    let value = serde_json::json!({
        "run_id": "run_1",
        "session_id": "session_1",
        "task_id": "task_1",
        "status": "completed",
        "final_answer": "done",
        "trace_path": "run_1",
        "events": [
            {
                "event_id": "event_1",
                "event_type": "lifecycle.run.started",
                "summary": "started",
                "component": "kernel",
                "severity": "info",
                "sequence": 0,
                "raw_prompt": "do not project",
                "raw_response": "do not project",
                "raw_arguments": {"path": ".env"},
                "provider_response": {"token": "secret"},
                "metadata": {"api_key": "secret"}
            }
        ],
        "raw_prompt": "do not project",
        "raw_response": "do not project",
        "raw_arguments": {"path": ".env"},
        "provider_response": {"token": "secret"},
        "metadata": {"api_key": "secret"}
    });

    let result: PythonSidecarRunResult =
        serde_json::from_value(value).expect("unknown sidecar fields are ignored");
    let bridge = AgentLoopStatusBridge::from_sidecar(result);
    let summary = sidecar_trace_summary(&bridge);
    let summary_text = summary.to_string().to_lowercase();

    assert_eq!(bridge.status, AgentHostStatus::Completed);
    assert_eq!(bridge.events.len(), 1);
    for marker in [
        "raw_prompt",
        "raw_response",
        "raw_arguments",
        "provider_response",
        "metadata",
        "api_key",
        "token",
        "secret",
    ] {
        assert!(
            !summary_text.contains(marker),
            "sidecar trace summary leaked {marker}: {summary_text}"
        );
    }
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
fn sidecar_cancel_and_status_return_typed_safe_envelopes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_cancel_status.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    method = message["method"]
    params = message.get("params") or {}
    if method == "agent/cancel":
        result = {
            "run_id": params["runId"],
            "status": "cancel_requested",
            "raw_prompt": "do not project",
        }
    elif method == "agent/status":
        result = {
            "run_id": params["runId"],
            "status": "running",
            "raw_response": "do not project",
        }
    else:
        result = {"run_id": "unexpected", "status": "failed"}
    print(json.dumps({"id": message["id"], "result": result}), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_cancel_status".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let cancel = client.cancel("run_1").expect("cancel");
    let status = client.status("run_1").expect("status");

    assert_eq!(cancel.run_id, "run_1");
    assert_eq!(cancel.status, "cancel_requested");
    assert_eq!(status.run_id, "run_1");
    assert_eq!(status.status, "running");
}

#[test]
fn sidecar_cancel_status_reject_malformed_response() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_malformed_cancel.py"),
        r#"
import json
import sys

for line in sys.stdin:
    message = json.loads(line)
    print(json.dumps({"id": message["id"], "result": {"status": "running"}}), flush=True)
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_malformed_cancel".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let error = client
        .cancel("run_1")
        .expect_err("missing run_id should be invalid");

    assert!(error.contains("invalid sidecar cancel result"));
}

#[test]
fn sidecar_status_reports_stdout_eof_as_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_eof.py"),
        "import sys\nsys.exit(0)\n",
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_eof".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn(&config).expect("spawn sidecar");

    let error = client
        .status("run_1")
        .expect_err("stdout EOF should be reported");

    assert!(
        error.contains("Python sidecar closed stdout")
            || error.contains("Python sidecar exited before response"),
        "unexpected sidecar EOF error: {error}"
    );
}

#[test]
fn sidecar_status_times_out_and_terminates_hung_sidecar() {
    let dir = tempfile::tempdir().expect("temp dir");
    let module_root = dir.path().join("sidecar_modules");
    std::fs::create_dir_all(&module_root).expect("module root");
    std::fs::write(
        module_root.join("sidecar_hang.py"),
        r#"
import json
import sys
import threading

for line in sys.stdin:
    json.loads(line)
    threading.Event().wait()
"#,
    )
    .expect("sidecar module");
    let config = PythonSidecarConfig {
        python_bin: "python".to_string(),
        module: "sidecar_hang".to_string(),
        project_root: dir.path().to_path_buf(),
        python_path: Some(module_root),
        env: Vec::new(),
    };
    let mut client = PythonSidecarClient::spawn_with_response_timeout(
        &config,
        Duration::from_millis(TEST_SIDECAR_RESPONSE_TIMEOUT_MS),
    )
    .expect("spawn sidecar");

    let error = client
        .status("run_1")
        .expect_err("hung sidecar should time out");

    assert!(error.contains("timed out waiting for Python sidecar response"));
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
