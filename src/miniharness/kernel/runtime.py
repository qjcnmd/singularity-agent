from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from rich.console import Console

from miniharness.agent import MiniAgent

from miniharness.kernel.cancellation import CancellationManager
from miniharness.kernel.exceptions import CancellationError, KernelError
from miniharness.kernel.finalization import FinalReport, KernelFinalizer
from miniharness.kernel.graph import RuntimeGraph
from miniharness.kernel.health import RuntimeHealthChecker, RuntimeHealthReport
from miniharness.kernel.lifecycle import RunLifecycleManager
from miniharness.kernel.locks import WorkspaceLockManager
from miniharness.kernel.models import (
    CancellationReason,
    KernelContext,
    KernelStatus,
    RunStatus,
    ShutdownReason,
)
from miniharness.kernel.recovery import CrashRecoveryManager, RecoveryReport
from miniharness.kernel.shutdown import ShutdownManager, ShutdownSummary


@dataclass(frozen=True)
class AgentRunResult:
    final_answer: str
    final_report: FinalReport
    status: RunStatus

    def to_dict(self) -> dict[str, Any]:
        return {
            "final_answer": self.final_answer,
            "final_report": self.final_report.to_dict(),
            "status": self.status.value,
        }


class AgentKernel:
    def __init__(
        self,
        *,
        context: KernelContext,
        graph: RuntimeGraph,
        lifecycle: RunLifecycleManager,
        workspace_lock: WorkspaceLockManager,
        cancellation: CancellationManager | None = None,
        console: Console | None = None,
        recovery_report: RecoveryReport | None = None,
        health_report: RuntimeHealthReport | None = None,
    ) -> None:
        self.context = context
        self.graph = graph
        self.lifecycle = lifecycle
        self.workspace_lock = workspace_lock
        self.cancellation = cancellation or CancellationManager()
        self.console = console or Console()
        self.recovery_report = recovery_report
        self.health_report = health_report
        self.shutdown_summary: ShutdownSummary | None = None
        self._final_report: FinalReport | None = None
        self._finalizing_during_shutdown = False
        for runtime in (
            self.graph.planner,
            self.graph.model_runtime,
            self.graph.command_runtime,
            self.graph.sandbox_runtime,
            self.graph.verification_runtime,
        ):
            setattr(runtime, "cancellation_token", self.cancellation.child_token())

    def boot(self) -> "AgentKernel":
        self.context.status = KernelStatus.READY
        return self

    def run_task(self, user_goal: str) -> AgentRunResult:
        self.context.status = KernelStatus.RUNNING
        self.lifecycle.start_task(user_goal)
        try:
            self.cancellation.throw_if_cancelled()
            agent = MiniAgent(
                model_runtime=self.graph.model_runtime,
                tools=self.graph.tools,
                trace=self.graph.trace,
                console=self.console,
                max_turns=self.graph.config.max_turns,
                planner=self.graph.planner,
                policy_runtime=self.graph.policy_runtime,
                tool_runtime=self.graph.tool_runtime,
                protocol_runtime=self.graph.protocol_runtime,
                instruction_runtime=self.graph.instruction_runtime,
                context_manager=getattr(self.graph, "context_manager", None),
                context_db_path=self.graph.config.context_db_path(self.graph.trace.store.run_dir),
                strict=self.graph.config.strict,
            )
            final_answer = agent.run(user_goal)
            self.graph.workspace_state.record_external_changes()
            self.lifecycle.mark_completed(final_answer)
            self.shutdown(ShutdownReason.NORMAL)
            report = self.final_report()
            return AgentRunResult(
                final_answer=final_answer,
                final_report=report,
                status=RunStatus.COMPLETED,
            )
        except KeyboardInterrupt:
            self.cancel(CancellationReason.USER_INTERRUPTED, "KeyboardInterrupt")
            self.lifecycle.mark_cancelled("KeyboardInterrupt")
            self.shutdown(ShutdownReason.KEYBOARD_INTERRUPT)
            self._finalize_after_shutdown("keyboard_interrupt")
            raise CancellationError("Cancelled by KeyboardInterrupt.", code="keyboard_interrupt")
        except CancellationError:
            self.lifecycle.mark_cancelled("cancelled")
            self.shutdown(ShutdownReason.CANCELLED)
            self._finalize_after_shutdown("cancelled")
            raise
        except Exception as exc:
            self.lifecycle.mark_failed(exc)
            self.context.diagnostics.append(
                {"type": type(exc).__name__, "message": str(exc)}
            )
            self.shutdown(ShutdownReason.ERROR)
            self._finalize_after_shutdown("error")
            if isinstance(exc, KernelError):
                raise
            raise

    def cancel(
        self,
        reason: CancellationReason = CancellationReason.SHUTDOWN_REQUESTED,
        message: str = "",
    ) -> None:
        self.context.status = KernelStatus.CANCELLING
        self.cancellation.cancel(reason, message)
        self.graph.trace.record(
            "cancellation.requested",
            {"reason": reason.value, "message": message},
        )

    def shutdown(self, reason: ShutdownReason = ShutdownReason.NORMAL) -> ShutdownSummary:
        if self.shutdown_summary is not None:
            return self.shutdown_summary
        self.context.status = KernelStatus.SHUTTING_DOWN
        if not self.cancellation.token.cancelled:
            self.cancel(CancellationReason.SHUTDOWN_REQUESTED, reason.value)
        manager = ShutdownManager(
            planner=self.graph.planner,
            model=self.graph.model_runtime,
            command=self.graph.command_runtime,
            sandbox=self.graph.sandbox_runtime,
            mutation=self.graph.mutation_runtime,
            workspace_state=self.graph.workspace_state,
            trace=self.graph.trace,
            workspace_lock=self.workspace_lock,
            final_report_writer=self._write_partial_final_report,
        )
        self.shutdown_summary = manager.shutdown(reason)
        self.context.workspace_lock_status = "released"
        self._final_report = None
        self.final_report()
        return self.shutdown_summary

    def recover_previous_run(self) -> RecoveryReport:
        report = CrashRecoveryManager(
            trace=self.graph.trace,
            workspace_lock=self.workspace_lock,
            workspace_state=self.graph.workspace_state,
            sandbox=self.graph.sandbox_runtime,
            command=self.graph.command_runtime,
        ).recover()
        self.recovery_report = report
        self.context.recovered_previous_run = report.recovered
        return report

    def health_check(self) -> RuntimeHealthReport:
        checker = RuntimeHealthChecker(trace=self.graph.trace)
        self.health_report = checker.check(self.graph.components_for_health())
        return self.health_report

    def final_report(self) -> FinalReport:
        if self._final_report is not None:
            return self._final_report
        planner_report = None
        if self.graph.planner.final_report is not None:
            planner_report = self.graph.planner.final_report
        elif self.graph.planner.state is not None:
            try:
                planner_report = self.graph.planner.finalize()
            except Exception as exc:
                self.context.diagnostics.append(
                    {"type": type(exc).__name__, "message": str(exc), "stage": "planner_finalize"}
                )
        workspace_health = self.graph.workspace_state.get_workspace_health()
        trace_summary = {}
        if hasattr(self.graph.trace, "final_report_summary"):
            trace_summary = self.graph.trace.final_report_summary(task_id=self.context.identity.task_id)
        self._final_report = KernelFinalizer().finalize(
            context=self.context,
            planner_report=planner_report,
            runtime_health_summary=(self.health_report.to_dict() if self.health_report else {}),
            shutdown_summary=self.shutdown_summary,
            recovery_summary=(self.recovery_report.to_dict() if self.recovery_report else {}),
            lifecycle_summary=self.lifecycle.summary(),
            config_summary={
                "max_turns": self.graph.config.max_turns,
                "profile": self.graph.config.profile,
                "approval_mode": self.graph.config.approval_mode.value,
                "strict": self.graph.config.strict,
                "dry_run": self.graph.config.dry_run,
                "raw_artifacts": self.graph.config.raw_artifacts,
            },
            workspace_summary=workspace_health.to_dict(),
            trace_summary=trace_summary,
        )
        self.context.status = KernelStatus.FINALIZED
        self.graph.trace.record("finalization.completed", self._final_report.to_dict())
        return self._final_report

    def _finalize_after_shutdown(self, stage: str) -> None:
        try:
            self.final_report()
        except Exception as exc:
            self.context.diagnostics.append(
                {
                    "type": type(exc).__name__,
                    "message": str(exc),
                    "stage": f"{stage}_finalization",
                }
            )

    def _write_partial_final_report(self) -> None:
        if self._finalizing_during_shutdown:
            return
        self._finalizing_during_shutdown = True
        try:
            self.final_report()
        finally:
            self._finalizing_during_shutdown = False
