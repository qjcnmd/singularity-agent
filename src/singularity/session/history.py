from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from singularity.context.store import ObservationStore
from singularity.observability.models import TraceArtifact, TraceEvent, TraceSpan
from singularity.observability.summary import TraceSummaryBuilder
from singularity.planner.store import PlannerStore
from singularity.session.models import SessionCheckpointKind, SessionResumeContext
from singularity.session.store import SessionStore
from singularity.tool_protocol.recovery import ToolProtocolRecoveryManager
from singularity.workspace_state import WorkspaceHealthReport


class SessionHistoryReader:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)

    def build_resume_context(
        self,
        *,
        session_id: str,
        user_goal: str,
        workspace_health: WorkspaceHealthReport,
        current_run_id: str,
        task_id: str,
        trace: Any | None = None,
        tool_protocol_state_path: Path | None = None,
        tool_protocol_run_id: str | None = None,
    ) -> SessionResumeContext:
        detail = self._session_detail(session_id)
        previous_run = _previous_run(detail, current_run_id)
        planner = self._planner_state(session_id)
        stable_goal = (
            str(planner.get("effective_goal") or planner.get("user_goal") or "")
            if planner.get("status") != "missing"
            else ""
        )
        if not stable_goal and detail is not None:
            stable_goal = detail.session.user_goal
        stable_goal = stable_goal or user_goal
        tool_protocol = self._tool_protocol_report(
            run_id=tool_protocol_run_id or current_run_id,
            session_id=session_id,
            task_id=task_id,
            state_path=tool_protocol_state_path,
        )
        verification = self._verification_summary(
            trace=trace,
            run_id=current_run_id,
            task_id=task_id,
        )
        previous_trace = self._previous_trace_summary(previous_run)
        if previous_trace:
            verification = {**verification, "previous_trace": previous_trace}
        failures = self._failure_summary(detail)
        dialogue = self._context_dialogue(previous_run)
        if not dialogue and detail is not None:
            dialogue = [
                {"role": "user", "content": run.user_goal}
                for run in detail.runs[-6:]
                if run.user_goal
            ]
        return SessionResumeContext.from_sources(
            session_id=session_id,
            user_goal=stable_goal,
            current_instruction=user_goal if user_goal != stable_goal else "",
            dialogue=dialogue,
            planner=planner,
            workspace=workspace_health.to_dict(),
            verification=verification,
            tool_protocol=tool_protocol,
            failures=failures,
        )

    def planner_state(self, session_id: str) -> dict[str, Any]:
        return self._planner_state(session_id)

    def build_show_summary(self, session_id: str) -> dict[str, Any]:
        detail = self._session_detail(session_id)
        if detail is None:
            raise KeyError(session_id)
        latest_run = detail.runs[-1] if detail.runs else None
        workspace_checkpoint = _latest_checkpoint(detail, SessionCheckpointKind.WORKSPACE)
        recovery_gate_checkpoint = _latest_checkpoint(
            detail,
            SessionCheckpointKind.RECOVERY_GATE,
        )
        tool_protocol: dict[str, Any] = {"next_action": "request_model"}
        verification: dict[str, Any] = {}
        dialogue: list[dict[str, str]] = []
        if latest_run is not None:
            dialogue = self._context_dialogue(latest_run)
            tool_protocol = self._tool_protocol_report(
                run_id=latest_run.run_id,
                session_id=session_id,
                task_id=latest_run.task_id,
                state_path=Path(latest_run.trace_run_dir) / "tool_protocol.sqlite3",
            )
            verification = self._previous_trace_summary(latest_run)
        if not dialogue:
            dialogue = [
                {"role": "user", "content": run.user_goal}
                for run in detail.runs[-6:]
                if run.user_goal
            ]
        return {
            "dialogue_summary": dialogue,
            "planner": self._planner_state(session_id),
            "workspace": workspace_checkpoint.payload if workspace_checkpoint else {},
            "verification": verification,
            "tool_protocol": tool_protocol,
            "failures": self._failure_summary(detail),
            "last_recovery_gate": (
                recovery_gate_checkpoint.payload if recovery_gate_checkpoint else {}
            ),
        }

    def tool_protocol_report(
        self,
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        state_path: Path | None,
    ) -> dict[str, Any]:
        return self._tool_protocol_report(
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            state_path=state_path,
        )

    def _session_detail(self, session_id: str):
        store = SessionStore(self.workspace_root)
        try:
            return store.show_session(session_id)
        except KeyError:
            return None
        finally:
            store.close()

    def _planner_state(self, session_id: str) -> dict[str, Any]:
        store = PlannerStore(self.workspace_root)
        try:
            state, plan, evidence, _budget, _final_report = store.load(session_id)
        except (FileNotFoundError, KeyError, ValueError):
            return {"status": "missing", "blockers": ["planner_state_missing"]}
        return {
            "task_id": state.task_id,
            "status": state.status.value,
            "current_phase": state.current_phase,
            "user_goal": state.user_goal,
            "effective_goal": state.effective_goal or state.normalized_goal,
            "goal_revisions": state.goal_revisions[-5:],
            "blocked_reasons": state.blocked_reasons[-10:],
            "linked_transactions": state.linked_transactions[-20:],
            "linked_commands": state.linked_commands[-20:],
            "linked_verifications": state.linked_verifications[-20:],
            "rolling_plan": {
                "current_phase": getattr(plan, "current_phase", None),
                "phase_count": len(getattr(plan, "phases", []) or []),
            },
            "verification_results_count": len(evidence.verification_results),
            "tool_results_count": len(evidence.tool_results),
            "external_changes": list(evidence.external_changes[-20:]),
        }

    def _tool_protocol_report(
        self,
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        state_path: Path | None,
    ) -> dict[str, Any]:
        if state_path is None or not state_path.exists():
            return {"next_action": "request_model"}
        try:
            return ToolProtocolRecoveryManager(state_path).inspect(
                run_id=run_id,
                session_id=session_id,
                task_id=task_id,
            ).to_dict()
        except Exception as exc:
            return {
                "next_action": "blocked",
                "warnings": [f"tool protocol inspect failed: {type(exc).__name__}"],
            }

    @staticmethod
    def _verification_summary(*, trace: Any | None, run_id: str, task_id: str) -> dict[str, Any]:
        if trace is None or not hasattr(trace, "final_report_summary"):
            return {}
        try:
            summary = trace.final_report_summary(run_id=run_id, task_id=task_id)
        except Exception:
            return {}
        return {
            "verification_checks": summary.get("verification_checks"),
            "failed_actions": summary.get("failed_actions"),
            "key_failures": summary.get("key_failures", [])[:5],
        }

    @staticmethod
    def _failure_summary(detail: Any | None) -> dict[str, Any]:
        if detail is None:
            return {}
        failed_runs = [
            run
            for run in detail.runs
            if run.status.value in {"failed", "blocked", "interrupted", "needs_review"}
        ]
        if not failed_runs:
            return {}
        last = failed_runs[-1]
        return {
            "last_status": last.status.value,
            "last_run_id": last.run_id,
            "summary": last.summary,
        }

    def _context_dialogue(self, previous_run: Any | None) -> list[dict[str, str]]:
        if previous_run is None:
            return []
        context_db = Path(previous_run.trace_run_dir) / "context.sqlite3"
        if not context_db.exists():
            return []
        store = ObservationStore(context_db)
        try:
            messages = store.load_messages(previous_run.run_id)
            if messages:
                return _safe_dialogue(messages)
            items = store.query_items(run_id=previous_run.run_id)
            return _safe_context_item_dialogue(items)
        except Exception:
            return []
        finally:
            store.close()

    def _previous_trace_summary(self, previous_run: Any | None) -> dict[str, Any]:
        if previous_run is None:
            return {}
        run_dir = Path(previous_run.trace_run_dir)
        if not run_dir.exists():
            return {}
        try:
            summary = TraceSummaryBuilder().summarize(
                events=[
                    TraceEvent.from_dict(item)
                    for item in _read_jsonl(run_dir / "events.jsonl")
                ],
                spans=[
                    TraceSpan.from_dict(item)
                    for item in _read_jsonl(run_dir / "spans.jsonl")
                ],
                artifacts=[
                    TraceArtifact.from_dict(item)
                    for item in _read_jsonl(run_dir / "artifacts.jsonl")
                ],
                run_id=previous_run.run_id,
                task_id=previous_run.task_id,
            )
        except Exception:
            return {}
        return {
            "run_id": summary.run_id,
            "total_events": summary.total_events,
            "failed_actions": summary.failed_action_count,
            "commands": summary.command_count,
            "workspace_mutations": summary.mutation_count,
            "verification_checks": summary.verification_count,
            "policy_denials": summary.policy_denial_count,
            "key_artifacts": list(summary.key_artifacts[:10]),
        }


def _previous_run(detail: Any | None, current_run_id: str) -> Any | None:
    if detail is None:
        return None
    previous = [run for run in detail.runs if run.run_id != current_run_id]
    return previous[-1] if previous else None


def _latest_checkpoint(detail: Any, kind: SessionCheckpointKind) -> Any | None:
    for checkpoint in reversed(detail.checkpoints):
        if checkpoint.kind == kind:
            return checkpoint
    return None


def _safe_dialogue(messages: list[dict[str, Any]]) -> list[dict[str, str]]:
    dialogue: list[dict[str, str]] = []
    for message in messages:
        role = str(message.get("role") or "")
        if role not in {"user", "assistant"}:
            continue
        content = str(message.get("content") or "")
        if content:
            dialogue.append({"role": role, "content": content[:1000]})
    return dialogue[-12:]


def _safe_context_item_dialogue(items: list[Any]) -> list[dict[str, str]]:
    dialogue: list[dict[str, str]] = []
    for item in items:
        item_type = getattr(getattr(item, "item_type", None), "value", getattr(item, "item_type", ""))
        content = getattr(item, "content", None)
        if item_type == "user_message":
            text = _content_text(content)
            if text:
                dialogue.append({"role": "user", "content": text[:1000]})
        elif item_type in {"assistant_message", "summary", "session_resume_context"}:
            text = _content_text(content)
            if text:
                dialogue.append({"role": "assistant", "content": text[:1000]})
    return dialogue[-12:]


def _content_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, dict):
        for key in ("summary", "content", "text", "message"):
            value = content.get(key)
            if isinstance(value, str) and value:
                return value
    return ""


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows
