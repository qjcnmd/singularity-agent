from __future__ import annotations

from typing import Any

from singularity.session.models import (
    RecoveryGateDecision,
    RecoveryGateStatus,
    SessionResumeContext,
)
from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


class SessionRecoveryGate:
    def evaluate(
        self,
        *,
        session_id: str,
        mode: str,
        workspace_health: WorkspaceHealthReport,
        crash_recovery: Any,
        tool_protocol_report: dict[str, Any] | None,
        context_recovery: dict[str, Any] | None,
        planner_state: dict[str, Any] | None,
        resume_context: SessionResumeContext | None = None,
    ) -> RecoveryGateDecision:
        blockers: list[str] = []
        warnings: list[str] = []
        if workspace_health.external_changes:
            blockers.append("external_user_change")
        if workspace_health.rollback_conflicts:
            blockers.append("rollback_conflict")
        if workspace_health.status == WorkspaceHealthStatus.CORRUPTED:
            blockers.append("corrupted_workspace_state")
        if crash_recovery.unfinished_mutations:
            blockers.append("unfinished_mutation")
        if crash_recovery.leftover_sandboxes:
            blockers.append("leftover_sandbox")
        if crash_recovery.stale_lock_detected:
            blockers.append("stale_lock_detected")
        tool_report = tool_protocol_report or {}
        if tool_report.get("pending_approval_call_ids") or str(tool_report.get("next_action")) == "resume_pending_approval":
            blockers.append("pending_approval")
        if tool_report.get("running_call_ids") or str(tool_report.get("next_action")) == "await_tool_result":
            blockers.append("running_tool_call")
        if tool_report.get("pending_call_ids") or str(tool_report.get("next_action")) == "execute_pending_tool":
            blockers.append("pending_tool_call")
        context_report = context_recovery or {}
        if context_report.get("context_recovery_failed"):
            blockers.append("context_recovery_failed")
        if context_report.get("open_mutation_transactions"):
            blockers.append("unfinished_mutation")
        if context_report.get("pending_policy_approval"):
            blockers.append("pending_approval")
        if context_report.get("pending_tool_calls"):
            blockers.append("pending_tool_call")
        if context_report.get("active_process_sessions"):
            blockers.append("running_tool_call")
        warnings.extend(str(item) for item in context_report.get("recovery_warnings") or [])
        planner_report = planner_state or {}
        if mode != "new" and (
            planner_report.get("status") == "missing"
            or "planner_state_missing" in list(planner_report.get("blockers") or [])
        ):
            blockers.append("planner_state_missing")
        blockers = _ordered_unique(blockers)

        if any(item in blockers for item in {"unfinished_mutation", "leftover_sandbox", "running_tool_call"}):
            status = RecoveryGateStatus.BLOCKED
        elif blockers:
            status = RecoveryGateStatus.NEEDS_REVIEW
        elif mode == "resume":
            status = RecoveryGateStatus.READY_TO_RESUME
        else:
            status = RecoveryGateStatus.READY_TO_CONTINUE
        can_call_model = status in {
            RecoveryGateStatus.READY_TO_CONTINUE,
            RecoveryGateStatus.READY_TO_RESUME,
        }
        next_action = (
            "continue"
            if can_call_model
            else f"run sg session show {session_id} --timeline and review recovery blockers"
        )
        resolved_context = resume_context or SessionResumeContext.from_sources(
            session_id=session_id,
            user_goal=str((planner_state or {}).get("user_goal") or ""),
            planner=planner_state or {},
            workspace=workspace_health.to_dict(),
            verification={"last_status": context_report.get("last_verification_status")},
            tool_protocol=tool_report,
            failures={"warnings": warnings, "blockers": blockers},
        )
        resolved_context = SessionResumeContext.from_sources(
            session_id=resolved_context.session_id,
            user_goal=resolved_context.user_goal,
            current_instruction=resolved_context.current_instruction,
            dialogue=resolved_context.dialogue_summary,
            planner=resolved_context.planner,
            workspace=resolved_context.workspace,
            verification=resolved_context.verification,
            tool_protocol=resolved_context.tool_protocol,
            failures={
                **resolved_context.failures,
                "warnings": warnings,
                "blockers": blockers,
            },
        )
        return RecoveryGateDecision(
            session_id=session_id,
            mode=mode,
            status=status,
            can_call_model=can_call_model,
            blockers=blockers,
            warnings=warnings,
            next_action=next_action,
            resume_context=resolved_context,
        )


def _ordered_unique(values: list[str]) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for value in values:
        if value in seen:
            continue
        seen.add(value)
        result.append(value)
    return result
