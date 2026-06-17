from __future__ import annotations

import json

from miniharness.planner.models import EvidenceLedger, TaskPlan, TaskState


class PlannerContextRenderer:
    def render(
        self,
        *,
        state: TaskState,
        plan: TaskPlan,
        evidence: EvidenceLedger,
    ) -> str:
        payload = {
            "planner": {
                "task_id": state.task_id,
                "session_id": state.session_id,
                "phase": state.current_phase,
                "status": state.status.value,
                "risk_level": state.risk_level.value,
                "allowed_tools": plan.phase(state.current_phase).allowed_tools,
                "evidence": {
                    "inspected_files": evidence.inspected_files[-20:],
                    "changed_files": self._changed_files(evidence),
                    "latest_verification": (
                        evidence.verification_results[-1]
                        if evidence.verification_results
                        else None
                    ),
                    "unresolved_failures": evidence.unresolved_failures[-10:],
                    "risks": evidence.risks[-10:],
                    "external_changes": evidence.external_changes[-20:],
                },
            }
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    @staticmethod
    def _changed_files(evidence: EvidenceLedger) -> list[str]:
        changed: set[str] = set()
        for change in evidence.applied_changes:
            for path in change.get("changed_files") or []:
                changed.add(str(path))
        return sorted(changed)
