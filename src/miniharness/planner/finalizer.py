from __future__ import annotations

from typing import Any

from miniharness.planner.models import EvidenceLedger, FinalReport, TaskState, TaskStatus


class Finalizer:
    def build(
        self,
        *,
        state: TaskState,
        evidence: EvidenceLedger,
        trace_summary: dict[str, Any] | None = None,
    ) -> FinalReport:
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
            sandbox = ((command.get("isolation_report") or {}).get("sandbox") or {})
            for item in sandbox.get("artifacts") or []:
                if item.get("relative_path"):
                    artifacts.add(str(item["relative_path"]))

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
            policy_approval_summary=self._policy_summary(evidence),
            artifacts=sorted(artifacts),
            next_steps=next_steps,
            sandbox_isolation_summary=self._sandbox_summary(evidence),
            execution_trace_summary=trace_summary
            or self._execution_trace_summary(evidence),
            model_usage_summary=(
                (trace_summary or {}).get("model_usage_summary")
                if trace_summary
                else {}
            ),
        )

    @staticmethod
    def _policy_summary(evidence: EvidenceLedger) -> dict[str, Any]:
        observations = evidence.policy_observations
        allowed = [item for item in observations if item.get("outcome") == "allow"]
        reviewed = [
            item
            for item in observations
            if item.get("outcome") in {"require_review", "reviewed", "approved"}
        ]
        denied = [item for item in observations if item.get("outcome") == "deny"]
        sandbox = [
            item for item in observations if item.get("outcome") == "sandbox_required"
        ]
        approved = [
            item
            for item in observations
            if item.get("approved_by_user") or item.get("approval_grant_id")
        ]
        high_risk_commands = [
            item
            for item in observations
            if item.get("runtime") == "command"
            and item.get("risk_level") in {"high", "critical"}
        ]
        skipped = [
            item
            for item in observations
            if item.get("outcome") in {"deny", "sandbox_required", "escalate"}
        ]
        return {
            "allowed_low_risk_actions_count": len(
                [item for item in allowed if item.get("risk_level") in {None, "none", "low"}]
            ),
            "reviewed_actions_count": len(reviewed),
            "denied_actions_count": len(denied),
            "sandbox_required_actions_count": len(sandbox),
            "user_approved_actions": approved,
            "high_risk_commands": high_risk_commands,
            "skipped_actions_due_to_policy": len(skipped),
        }

    @staticmethod
    def _sandbox_summary(evidence: EvidenceLedger) -> dict[str, Any]:
        observations = evidence.sandbox_observations
        if not observations:
            observations = []
            for command in evidence.command_results:
                sandbox = ((command.get("isolation_report") or {}).get("sandbox") or {})
                if sandbox.get("sandbox_id"):
                    observations.append(
                        {
                            "source": "command",
                            "status": sandbox.get("status"),
                            "artifact_count": sandbox.get("artifact_count", 0),
                            "changed_files_count": sandbox.get("changed_files_count", 0),
                            "violations": sandbox.get("violations") or [],
                            "imported_changes_count": sandbox.get("imported_changes_count", 0),
                        }
                    )
            for verification in evidence.verification_results:
                for result in verification.get("results") or []:
                    result_evidence = result.get("evidence") or {}
                    if result_evidence.get("sandbox_id"):
                        observations.append(
                            {
                                "source": "verification",
                                "status": result_evidence.get("sandbox_status"),
                                "artifact_count": len(result_evidence.get("sandbox_artifacts") or []),
                                "changed_files_count": (result_evidence.get("sandbox_changed_files") or {}).get("total_changed_files", 0),
                                "violations": result_evidence.get("sandbox_violations") or [],
                                "imported_changes_count": 0,
                            }
                        )
        return {
            "sandboxed_commands_count": len([item for item in observations if item.get("source") == "command"]),
            "verification_commands_run_in_sandbox_count": len([item for item in observations if item.get("source") == "verification"]),
            "backend_unavailable_count": len([item for item in observations if item.get("status") == "backend_unavailable"]),
            "sandbox_violation_count": sum(len(item.get("violations") or []) for item in observations),
            "timeout_count": len([item for item in observations if item.get("status") == "timeout"]),
            "artifact_count": sum(int(item.get("artifact_count") or 0) for item in observations),
            "changed_files_in_sandbox_count": sum(int(item.get("changed_files_count") or 0) for item in observations),
            "imported_changes_count": sum(int(item.get("imported_changes_count") or 0) for item in observations),
        }

    @staticmethod
    def _execution_trace_summary(evidence: EvidenceLedger) -> dict[str, Any]:
        failed_actions = [
            item
            for item in evidence.tool_results
            if item.get("ok") is False or item.get("error_code")
        ]
        failed_commands = [
            item
            for item in evidence.command_results
            if item.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}
        ]
        return {
            "total_actions": len(evidence.tool_results),
            "failed_actions": len(failed_actions),
            "tool_calls": len(evidence.tool_results),
            "commands_executed": len(evidence.command_results),
            "sandboxed_commands": len(evidence.sandbox_observations),
            "workspace_mutations": len(evidence.applied_changes),
            "verification_checks": sum(
                len(item.get("check_status") or [])
                for item in evidence.verification_results
            ),
            "policy_denials": len(
                [
                    item
                    for item in evidence.policy_observations
                    if item.get("outcome") in {"deny", "require_review", "sandbox_required", "escalate"}
                ]
            ),
            "approvals": len(
                [
                    item
                    for item in evidence.policy_observations
                    if item.get("approval_grant_id") or item.get("approved_by_user")
                ]
            ),
            "replans": 0,
            "key_failures": [
                str(item.get("summary") or item.get("error_code") or item)
                for item in [*failed_actions, *failed_commands]
            ][:10],
            "key_artifacts": [
                artifact
                for artifact in {
                    *[
                        str(item.get("artifact_path"))
                        for item in evidence.applied_changes
                        if item.get("artifact_path")
                    ],
                    *[
                        str(item.get("artifact_path"))
                        for item in evidence.command_results
                        if item.get("artifact_path")
                    ],
                }
                if artifact
            ],
        }
