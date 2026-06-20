from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from rich.console import Console

from miniharness.agent import MiniAgent
from miniharness.agent import MiniAgentRunStatus
from miniharness.interaction import (
    ControlCommand,
    FinalReport as InteractionFinalReport,
    InteractionRuntime,
)

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
    interaction_report: InteractionFinalReport | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "final_answer": self.final_answer,
            "final_report": self.final_report.to_dict(),
            "status": self.status.value,
            "interaction_report": (
                self.interaction_report.to_dict() if self.interaction_report else None
            ),
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
        self._interaction_report: InteractionFinalReport | None = None
        self._finalizing_during_shutdown = False
        self.interaction_runtime = getattr(self.graph, "interaction_runtime", None)
        if self.interaction_runtime is None:
            self.interaction_runtime = InteractionRuntime(
                trace=getattr(self.graph, "trace", None),
                cancellation_manager=self.cancellation,
            )
            try:
                setattr(self.graph, "interaction_runtime", self.interaction_runtime)
            except Exception:
                pass
        self.interaction_runtime.cancellation_manager = self.cancellation
        for runtime in (
            self.graph.planner,
            self.graph.model_runtime,
            self.graph.tool_runtime,
            self.graph.protocol_runtime,
            self.graph.command_runtime,
            self.graph.sandbox_runtime,
            self.graph.verification_runtime,
            self.graph.review_runtime,
            self.graph.context_manager,
            getattr(self.graph, "edit_runtime", None),
        ):
            if runtime is not None:
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
                interaction_runtime=self.interaction_runtime,
                context_manager=getattr(self.graph, "context_manager", None),
                context_db_path=self.graph.config.context_db_path(self.graph.trace.store.run_dir),
                strict=self.graph.config.strict,
            )
            agent_result = agent.run(user_goal)
            final_answer = str(agent_result.final_answer)
            self.graph.workspace_state.record_external_changes()
            if agent_result.status == MiniAgentRunStatus.COMPLETED:
                self.lifecycle.mark_completed(final_answer)
                shutdown_reason = ShutdownReason.NORMAL
                result_status = RunStatus.COMPLETED
            else:
                self.lifecycle.mark_failed(
                    f"{agent_result.status.value}: {agent_result.error_code or final_answer}"
                )
                self.context.diagnostics.append(
                    {
                        "type": "MiniAgentRunStatus",
                        "status": agent_result.status.value,
                        "error_code": agent_result.error_code,
                        "message": final_answer,
                    }
                )
                shutdown_reason = ShutdownReason.ERROR
                result_status = RunStatus.FAILED
            self.shutdown(shutdown_reason)
            report = self.final_report()
            interaction_report = self.interaction_final_report()
            return AgentRunResult(
                final_answer=final_answer,
                final_report=report,
                status=result_status,
                interaction_report=interaction_report,
            )
        except KeyboardInterrupt:
            self.interaction_runtime.handle_command(
                ControlCommand.CANCEL,
                message="KeyboardInterrupt",
                cancellation_manager=self.cancellation,
            )
            self.cancellation.cancel(
                CancellationReason.USER_INTERRUPTED,
                "KeyboardInterrupt",
            )
            self.context.status = KernelStatus.CANCELLING
            self.lifecycle.mark_cancelled("KeyboardInterrupt")
            self.shutdown(ShutdownReason.KEYBOARD_INTERRUPT)
            self._finalize_after_shutdown(
                "keyboard_interrupt",
                cancelled=True,
                cancellation_reason="KeyboardInterrupt",
            )
            raise CancellationError("Cancelled by KeyboardInterrupt.", code="keyboard_interrupt")
        except CancellationError:
            self.interaction_runtime.handle_command(
                ControlCommand.CANCEL,
                message="cancelled",
                cancellation_manager=self.cancellation,
            )
            if not self.cancellation.token.cancelled:
                self.cancellation.cancel(CancellationReason.USER_INTERRUPTED, "cancelled")
            self.context.status = KernelStatus.CANCELLING
            self.lifecycle.mark_cancelled("cancelled")
            self.shutdown(ShutdownReason.CANCELLED)
            self._finalize_after_shutdown(
                "cancelled",
                cancelled=True,
                cancellation_reason="cancelled",
            )
            raise
        except Exception as exc:
            self.lifecycle.mark_failed(exc)
            self.context.diagnostics.append(
                {"type": type(exc).__name__, "message": str(exc)}
            )
            self.shutdown(ShutdownReason.ERROR)
            self._finalize_after_shutdown("error", error=exc)
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
                "security_mode": self.graph.config.security_mode.value,
                "interaction_mode": self.graph.config.interaction_mode.value,
                "strict": self.graph.config.strict,
                "dry_run": self.graph.config.dry_run,
                "raw_artifacts": self.graph.config.raw_artifacts,
                "project_index_enabled": self.graph.config.project_index_enabled,
                "project_index_db": str(self.graph.config.project_index_db_path()),
            },
            workspace_summary=workspace_health.to_dict(),
            trace_summary=trace_summary,
        )
        self.context.status = KernelStatus.FINALIZED
        self.graph.trace.record("finalization.completed", self._final_report.to_dict())
        self._record_memory_session_end(self._final_report, trace_summary=trace_summary)
        self._build_interaction_final_report(
            planner_report=planner_report,
            kernel_report=self._final_report,
        )
        return self._final_report

    def _record_memory_session_end(
        self,
        final_report: FinalReport,
        *,
        trace_summary: dict[str, Any],
    ) -> None:
        memory_runtime = getattr(self.graph, "memory_runtime", None)
        if memory_runtime is None or not hasattr(memory_runtime, "ingest_session_end"):
            return
        try:
            memory_runtime.ingest_session_end(
                final_reports=[final_report],
                trace_summary=trace_summary,
            )
        except Exception as exc:
            self.context.diagnostics.append(
                {"type": type(exc).__name__, "message": str(exc), "stage": "memory_ingest"}
            )

    def interaction_final_report(self) -> InteractionFinalReport | None:
        return self._interaction_report

    def _finalize_after_shutdown(
        self,
        stage: str,
        *,
        cancelled: bool = False,
        cancellation_reason: str | None = None,
        error: BaseException | None = None,
    ) -> None:
        try:
            kernel_report = self.final_report()
            self._build_interaction_final_report(
                kernel_report=kernel_report,
                cancelled=cancelled,
                cancellation_reason=cancellation_reason,
                error=error,
            )
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

    def _build_interaction_final_report(
        self,
        *,
        planner_report: Any | None = None,
        kernel_report: FinalReport | None = None,
        cancelled: bool = False,
        cancellation_reason: str | None = None,
        error: BaseException | None = None,
    ) -> InteractionFinalReport | None:
        if self._interaction_report is not None and not (cancelled or error):
            return self._interaction_report
        state = getattr(self.graph.planner, "state", None)
        if planner_report is None:
            planner_report = getattr(self.graph.planner, "final_report", None)
        blocked_reasons = list(getattr(state, "blocked_reasons", []) or [])
        verification_required = True
        if state is not None:
            verification_required = bool(
                state.completion_criteria.required_verifications_passed
            )
        run_status = self.context.run.status
        interaction_report = self.interaction_runtime.build_final_report(
            planner_report=planner_report,
            kernel_report=kernel_report,
            workspace_summary=(
                kernel_report.workspace_summary if kernel_report is not None else None
            ),
            trace_summary=(
                kernel_report.trace_summary if kernel_report is not None else None
            ),
            error=error or (self.context.run.error if run_status == RunStatus.FAILED else None),
            cancelled=cancelled or run_status == RunStatus.CANCELLED,
            cancellation_reason=cancellation_reason,
            blocked_reasons=blocked_reasons,
            verification_required=verification_required,
        )
        self._interaction_report = interaction_report
        return interaction_report
