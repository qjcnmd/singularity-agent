from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from singularity.evaluation.models import FailureCaseRecord


FAILURE_CASE_REPLAY_SCHEMA_VERSION = "evaluation.failure_case_replay/v1"


class FailureCaseReplayRunner:
    """Extract bounded replay records from evaluation failure reports.

    This runner is intentionally post-run extraction only. Targeted execution
    replay lives in ``TargetedFailureReplayRunner``.
    """

    def __init__(self, *, report_path: Path | str, regression_path: Path | str | None = None) -> None:
        self.report_path = Path(report_path)
        self.regression_path = Path(regression_path) if regression_path else None

    def extract(self, *, task_id: str | None = None) -> list[FailureCaseRecord]:
        report = json.loads(self.report_path.read_text(encoding="utf-8"))
        tasks = report.get("tasks") or []
        records: list[FailureCaseRecord] = []
        for task in tasks:
            if not isinstance(task, dict):
                continue
            if task_id is not None and task.get("task_id") != task_id:
                continue
            if _task_succeeded(task):
                continue
            records.append(self._record_from_task(task))
        return records

    def write(self, output_path: Path | str, *, task_id: str | None = None) -> list[FailureCaseRecord]:
        records = self.extract(task_id=task_id)
        payload = {
            "schema_version": FAILURE_CASE_REPLAY_SCHEMA_VERSION,
            "runner_mode": "post_run_failure_extraction",
            "targeted_replay_runner": "TargetedFailureReplayRunner",
            "source_report_path": str(self.report_path),
            "source_regression_path": str(self.regression_path or ""),
            "failure_count": len(records),
            "records": [record.to_dict() for record in records],
        }
        Path(output_path).write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return records

    def _record_from_task(self, task: dict[str, Any]) -> FailureCaseRecord:
        environment = task.get("reproducible_environment") or {}
        trace_path = str(task.get("trace") or "")
        return FailureCaseRecord(
            task_id=str(task.get("task_id") or ""),
            status=str(task.get("status") or ""),
            failure_category=str(task.get("failure_category") or ""),
            miscompletion_count=_int(task.get("miscompletion_count")),
            public_verification_passed=bool(task.get("public_verification_passed")),
            hidden_verification_passed=bool(task.get("hidden_verification_passed")),
            policy_blocks=_int(task.get("policy_blocks")),
            expected_file_changes=[
                str(item) for item in environment.get("expected_file_changes") or []
            ],
            files_changed=[str(item) for item in task.get("files_changed") or []],
            final_report_status=str(task.get("final_report_status") or ""),
            repair_attempt_count=_int(task.get("repair_attempt_count")),
            repair_execution_count=_int(task.get("repair_execution_count")),
            blocked_reason=str(task.get("blocked_reason") or ""),
            trace_path=trace_path,
            trace_artifact_refs=[str(item) for item in task.get("trace_artifact_refs") or []],
            contract_satisfaction=dict(task.get("contract_satisfaction") or {}),
            repair_telemetry=_repair_telemetry(task),
            verification=dict(task.get("verification_result") or task.get("task_verification_result") or {}),
            trace_summary=_trace_summary(Path(trace_path)),
            source_report_path=str(self.report_path),
            source_regression_path=str(self.regression_path or ""),
        )


def _task_succeeded(task: dict[str, Any]) -> bool:
    return bool(task.get("success") is True and task.get("miscompletion_count") in {0, None})


def _repair_telemetry(task: dict[str, Any]) -> dict[str, Any]:
    contract_satisfaction = task.get("contract_satisfaction")
    if isinstance(contract_satisfaction, dict):
        repair_phase = contract_satisfaction.get("repair_phase_contract_satisfaction")
        if isinstance(repair_phase, dict):
            return dict(repair_phase)
    legacy = task.get("repair_verification_contract")
    return dict(legacy) if isinstance(legacy, dict) else {}


def _trace_summary(trace_path: Path) -> dict[str, Any]:
    events_path = trace_path / "events.jsonl"
    if not events_path.exists():
        return {
            "events_path": str(events_path),
            "event_count": 0,
            "events_available": False,
        }
    event_count = 0
    failure_analysis_events = 0
    repair_events = 0
    final_report_outcome = ""
    blocked_reasons: list[str] = []
    phase_policy_blocks: list[dict[str, Any]] = []
    for line in events_path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        event_count += 1
        event_type = str(event.get("event_type") or "")
        payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
        if event_type.startswith("failure_analysis."):
            failure_analysis_events += 1
        if event_type.startswith("repair.") or event_type == "repair.signal_consumed":
            repair_events += 1
        if event_type == "final_report.completed":
            final_report = payload.get("final_report") if isinstance(payload, dict) else {}
            if isinstance(final_report, dict):
                final_report_outcome = str(final_report.get("outcome") or "")
                blocked_reasons = [str(item) for item in final_report.get("blocked_reasons") or []]
        if _is_phase_policy_block(event_type, payload):
            phase_policy_blocks.append(_phase_policy_block(event, payload))
    return {
        "events_path": str(events_path),
        "event_count": event_count,
        "events_available": True,
        "failure_analysis_event_count": failure_analysis_events,
        "repair_event_count": repair_events,
        "final_report_outcome": final_report_outcome,
        "blocked_reasons": blocked_reasons,
        "phase_policy_blocks": phase_policy_blocks[-5:],
    }


def _is_phase_policy_block(event_type: str, payload: dict[str, Any]) -> bool:
    if event_type not in {"action.proposed", "tool.dispatch.failed"}:
        return False
    text = json.dumps(payload, ensure_ascii=False, sort_keys=True)
    return "action_not_allowed" in text or "not allowed in phase" in text


def _phase_policy_block(event: dict[str, Any], payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "event_type": event.get("event_type"),
        "summary": event.get("summary"),
        "phase": payload.get("phase"),
        "reason": payload.get("reason") or payload.get("planner_reason"),
    }


def _int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0
