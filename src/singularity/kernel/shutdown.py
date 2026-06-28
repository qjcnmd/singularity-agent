from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from singularity.kernel.models import ShutdownReason


@dataclass(frozen=True)
class ShutdownSummary:
    reason: ShutdownReason
    cleanup_status: str
    steps: list[dict[str, Any]]

    def to_dict(self) -> dict[str, Any]:
        return {
            "reason": self.reason.value,
            "cleanup_status": self.cleanup_status,
            "steps": self.steps,
        }


class ShutdownManager:
    def __init__(
        self,
        *,
        planner: Any | None = None,
        model: Any | None = None,
        command: Any | None = None,
        sandbox: Any | None = None,
        mutation: Any | None = None,
        workspace_state: Any | None = None,
        trace: Any | None = None,
        workspace_lock: Any | None = None,
        final_report_writer: Callable[[], Any] | None = None,
    ) -> None:
        self.planner = planner
        self.model = model
        self.command = command
        self.sandbox = sandbox
        self.mutation = mutation
        self.workspace_state = workspace_state
        self.trace = trace
        self.workspace_lock = workspace_lock
        self.final_report_writer = final_report_writer
        self._reason = ShutdownReason.NORMAL

    def shutdown(self, reason: ShutdownReason = ShutdownReason.NORMAL) -> ShutdownSummary:
        steps: list[dict[str, Any]] = []
        self._reason = reason
        self._record("shutdown.started", {"reason": reason.value})
        for name, callback in self._steps():
            try:
                callback()
                steps.append({"step": name, "status": "completed"})
            except Exception as exc:
                steps.append(
                    {
                        "step": name,
                        "status": "failed",
                        "error_type": type(exc).__name__,
                        "message": str(exc),
                    }
                )
        cleanup_status = (
            "completed"
            if all(step["status"] == "completed" for step in steps)
            else "completed_with_errors"
        )
        summary = ShutdownSummary(reason=reason, cleanup_status=cleanup_status, steps=steps)
        self._record("shutdown.completed", summary.to_dict())
        return summary

    def _steps(self) -> list[tuple[str, Callable[[], None]]]:
        return [
            ("stop_planner", self._stop_planner),
            ("reject_actions", lambda: None),
            ("cancel_model", self._cancel_model),
            ("terminate_commands", self._terminate_commands),
            ("terminate_sandbox", self._terminate_sandbox),
            ("finalize_mutations", self._finalize_mutations),
            ("checkpoint", self._checkpoint),
            ("flush_trace", self._flush_trace),
            ("write_report", self._write_report),
            ("release_lock", self._release_lock),
        ]

    def _stop_planner(self) -> None:
        if self.planner is None:
            return
        if self._reason == ShutdownReason.NORMAL:
            return
        if hasattr(self.planner, "interrupt"):
            self.planner.interrupt("kernel_shutdown")
        elif hasattr(self.planner, "stop"):
            self.planner.stop()

    def _cancel_model(self) -> None:
        if self.model is not None and hasattr(self.model, "cancel"):
            self.model.cancel()
        elif self.model is not None and hasattr(self.model, "stop"):
            self.model.stop()

    def _terminate_commands(self) -> None:
        if self.command is None or not hasattr(self.command, "list_processes"):
            if self.command is not None and hasattr(self.command, "stop"):
                self.command.stop()
            return
        for session in self.command.list_processes():
            if getattr(session, "status", "") == "running":
                self.command.stop_process(session.process_id)

    def _terminate_sandbox(self) -> None:
        if self.sandbox is not None and hasattr(self.sandbox, "shutdown"):
            self.sandbox.shutdown()
        elif self.sandbox is not None and hasattr(self.sandbox, "stop"):
            self.sandbox.stop()

    def _finalize_mutations(self) -> None:
        if self.mutation is not None and hasattr(self.mutation, "finalize"):
            self.mutation.finalize()

    def _checkpoint(self) -> None:
        if self.workspace_state is not None and hasattr(self.workspace_state, "record_external_changes"):
            self.workspace_state.record_external_changes()

    def _flush_trace(self) -> None:
        store = getattr(self.trace, "store", None)
        if store is not None and hasattr(store, "_write_index"):
            store._write_index()

    def _write_report(self) -> None:
        if self.final_report_writer is not None:
            self.final_report_writer()

    def _release_lock(self) -> None:
        if self.workspace_lock is not None and hasattr(self.workspace_lock, "release_lock"):
            self.workspace_lock.release_lock()

    def _record(self, event: str, payload: dict[str, Any]) -> None:
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record(event, payload)
