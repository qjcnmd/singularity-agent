from __future__ import annotations

from singularity.evaluation.targeted_replay import TargetedFailureReplayRunner


def test_targeted_failure_replay_runner_activates_repair_chain_and_completes(tmp_path):
    runner = TargetedFailureReplayRunner(workspace_root=tmp_path, max_turns=6)

    result = runner.run_smoke()
    payload = result.to_dict()

    assert payload["entered_agent_loop"] is True
    assert payload["agent_loop_ref"] == "AgentLoop.run"
    assert payload["status"] == "completed"
    assert payload["completed"] is True
    assert payload["failure_trigger"] == "verification_failed"
    assert payload["failure_analysis_request_count"] == 1
    assert payload["failure_analysis_result_count"] == 1
    assert payload["repair_plan_count"] == 1
    assert payload["repair_contract_count"] == 1
    assert payload["repair_attempt_count"] == 1
    assert payload["repair_execution_count"] == 1
    assert payload["repairing_failures_seen"] is True
    assert payload["verification_contract_satisfaction"]["satisfied"] is True
    assert payload["repair_scope"]["target_file_scope_ok"] is True
    assert payload["repair_scope"]["verification_command_scope_ok"] is True
    assert payload["final_report_status"] == "completed"
    assert "FailureAnalysisRequest.to_model_payload" in payload["model_visible_objects"]
    assert "FailureCaseRecord" in payload["evaluator_internal_objects"]
