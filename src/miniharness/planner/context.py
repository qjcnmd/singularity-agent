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
                    "policy_observations": [
                        self._policy_summary(item)
                        for item in evidence.policy_observations[-10:]
                    ],
                    "sandbox_observations": [
                        str(item.get("summary") or self._sandbox_summary(item))
                        for item in evidence.sandbox_observations[-10:]
                    ],
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

    @staticmethod
    def _policy_summary(observation: dict) -> str:
        runtime = str(observation.get("runtime") or "policy").replace("_", " ").title()
        outcome = {
            "allow": "allowed",
            "deny": "denied",
            "require_review": "requires review",
            "sandbox_required": "requires sandbox",
            "escalate": "escalated",
            "ask_user": "needs input",
        }.get(str(observation.get("outcome") or ""), "blocked")
        reason = str(observation.get("reason") or "")
        return f"[policy] {runtime} {outcome}: {reason}"

    @staticmethod
    def _sandbox_summary(observation: dict) -> str:
        status = str(observation.get("status") or "unknown")
        backend = str(observation.get("backend") or "sandbox")
        if status == "backend_unavailable":
            return "[sandbox] command blocked: backend cannot enforce required isolation."
        return f"[sandbox] command ran in isolated copy-on-write workspace via {backend}, status={status}."
