from __future__ import annotations

from typing import Any

from miniharness.planner.models import EvidenceLedger, FinalReport, TaskState, TaskStatus


class Finalizer:
    def build(self, *, state: TaskState, evidence: EvidenceLedger) -> FinalReport:
        files_changed: set[str] = set()
        artifacts: set[str] = set()
        for change in evidence.applied_changes:
            for path in change.get("changed_files") or []:
                files_changed.add(str(path))
            artifact = change.get("artifact_path")
            if artifact:
                artifacts.add(str(artifact))
        for command in evidence.command_results:
            artifact = command.get("artifact_path")
            if artifact:
                artifacts.add(str(artifact))

        verification_summary: dict[str, Any] = {"status": "not_run"}
        if evidence.verification_results:
            latest = evidence.verification_results[-1]
            verification_summary = dict(latest.get("completion_assessment") or {})
            if "check_status" in latest:
                verification_summary["check_status"] = latest["check_status"]

        status = TaskStatus.COMPLETED if verification_summary.get("status") in {"ready", "ready_with_warnings"} else state.status
        next_steps = [] if status == TaskStatus.COMPLETED else ["Resolve unmet completion criteria."]

        return FinalReport(
            user_goal=state.user_goal,
            status=status,
            files_changed=sorted(files_changed),
            agent_changes=list(evidence.applied_changes),
            command_side_effects=list(evidence.command_results),
            verification_summary=verification_summary,
            unresolved_issues=list(evidence.unresolved_failures),
            risks=list(evidence.risks),
            rollback_status={"available": bool(files_changed), "transactions": state.linked_transactions},
            artifacts=sorted(artifacts),
            next_steps=next_steps,
        )
