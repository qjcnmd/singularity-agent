from __future__ import annotations

import json
from pathlib import Path

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
    assert "model_visible_objects" not in payload
    assert "evaluator_internal_objects" not in payload
    assert "repairing_failures" in payload["phase_history"]
    assert any(
        item["status"] == "repairing_failures"
        and item["current_phase"] == "repairing_failures"
        for item in payload["planner_status_history"]
    )
    assert payload["repairing_failures_evidence"]["seen"] is True
    assert {
        "planner_history",
        "trace_event",
    } <= set(payload["repairing_failures_evidence"]["sources"])
    assert payload["repair_contract_summary"]["target_files"] == ["quicksort.py"]
    assert payload["repair_contract_summary"]["verification_plan"] == ["python quicksort.py"]
    assert payload["trace_refs"]["jsonl_path"] == payload["trace_path"]
    assert payload["trace_refs"]["event_count"] > 0
    assert payload["trace_refs"]["repair_event_count"] >= 1
    trace_events = [
        json.loads(line)
        for line in Path(payload["trace_path"]).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert any(
        event.get("event") == "planner"
        and (event.get("data") or {}).get("phase") == "repairing_failures"
        and (event.get("data") or {}).get("decision") == "allow"
        and (event.get("data") or {}).get("action_kind") == "ApplyMutation"
        and "write_file is allowed" in str((event.get("data") or {}).get("reason") or "")
        for event in trace_events
    )


def test_targeted_failure_replay_runner_writes_bounded_json_and_markdown_reports(tmp_path):
    runner = TargetedFailureReplayRunner(workspace_root=tmp_path / "workspace", max_turns=6)
    output_dir = tmp_path / "reports"

    result = runner.run(output_dir=output_dir)
    rerun = runner.run(output_dir=output_dir)

    json_path = output_dir / "targeted_replay_result.json"
    markdown_path = output_dir / "targeted_replay_result.md"
    assert result.report_paths == {
        "json": str(json_path),
        "markdown": str(markdown_path),
    }
    assert rerun.completed is True
    payload = json.loads(json_path.read_text(encoding="utf-8"))
    markdown = markdown_path.read_text(encoding="utf-8")
    assert payload["status"] == "completed"
    assert payload["entered_agent_loop"] is True
    assert "model_visible_objects" not in payload
    assert "evaluator_internal_objects" not in payload
    assert len(payload["phase_history"]) <= 20
    assert len(payload["planner_status_history"]) <= 20
    assert "Targeted Failure Replay" in markdown
    assert "AgentLoop.run" in markdown
