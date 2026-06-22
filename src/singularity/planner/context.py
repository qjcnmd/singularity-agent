from __future__ import annotations

import json

from singularity.planner.models import EvidenceLedger, TaskPlan, TaskState


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
                "user_goal": state.user_goal,
                "effective_goal": state.effective_goal or state.normalized_goal,
                "goal_revisions": state.goal_revisions[-5:],
                "phase": state.current_phase,
                "status": state.status.value,
                "risk_level": state.risk_level.value,
                "allowed_tools": plan.phase(state.current_phase).allowed_tools,
                "task_contract": self._contract_summary(state.task_contract),
                "evidence": {
                    "inspected_files": evidence.inspected_files[-20:],
                    "changed_files": self._changed_files(evidence),
                    "latest_verification": (
                        evidence.verification_results[-1]
                        if evidence.verification_results
                        else None
                    ),
                    "unresolved_failures": evidence.unresolved_failures[-10:],
                    "missing_evidence": evidence.missing_evidence[-10:],
                    "task_outcomes": evidence.task_outcomes[-10:],
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
                    "project_index": self._project_index_summary(evidence),
                    "latest_review": (
                        evidence.review_results[-1]
                        if evidence.review_results
                        else None
                    ),
                },
            }
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    @staticmethod
    def _contract_summary(contract: dict) -> dict:
        if not contract:
            return {}
        return {
            "source": contract.get("source"),
            "acceptance_criteria": contract.get("acceptance_criteria") or [],
            "deliverables": contract.get("deliverables") or [],
            "verification_requirements": contract.get("verification_requirements") or [],
            "report_requirements": contract.get("report_requirements") or [],
            "evidence_requirements": contract.get("evidence_requirements") or [],
        }

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

    @staticmethod
    def _project_index_summary(evidence: EvidenceLedger) -> dict:
        if not evidence.project_index_observations:
            return {}
        latest = evidence.project_index_observations[-1]
        summary = latest.get("summary") or {}
        return {
            "index_id": latest.get("index_id"),
            "freshness": summary.get("freshness"),
            "file_count": summary.get("file_count"),
            "symbol_count": summary.get("symbol_count"),
            "entrypoint_count": summary.get("entrypoint_count"),
            "languages": summary.get("languages") or [],
            "relevant_files": [
                {
                    "path": item.get("path"),
                    "score": item.get("score"),
                    "reasons": item.get("reasons") or [],
                    "freshness": item.get("freshness"),
                }
                for item in (latest.get("relevant_files") or [])[:12]
            ],
            "warnings": latest.get("warnings") or [],
            "trust_level": latest.get("trust_level") or "untrusted_workspace_data",
        }
