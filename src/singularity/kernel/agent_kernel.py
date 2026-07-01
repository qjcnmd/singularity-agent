from __future__ import annotations

from contextlib import suppress
from dataclasses import dataclass
from typing import Any

from rich.console import Console

from singularity.agent_loop import AgentLoop, AgentLoopStatus
from singularity.interaction import (
    ControlCommand,
    InteractionController,
)
from singularity.interaction import (
    FinalReport as InteractionFinalReport,
)
from singularity.kernel.cancellation import CancellationManager
from singularity.kernel.exceptions import CancellationError, KernelError
from singularity.kernel.finalization import FinalReport, KernelFinalizer
from singularity.kernel.graph import AgentGraph
from singularity.kernel.health import ComponentHealthChecker, ComponentHealthReport
from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.locks import WorkspaceLockManager
from singularity.kernel.models import (
    CancellationReason,
    KernelContext,
    KernelStatus,
    RunStatus,
    ShutdownReason,
)
from singularity.kernel.recovery import CrashRecoveryManager, RecoveryReport
from singularity.kernel.shutdown import ShutdownManager, ShutdownSummary
from singularity.session.models import RecoveryGateDecision, RecoveryGateStatus


@dataclass(frozen=True)
class RunResult:
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
        graph: AgentGraph,
        lifecycle: RunLifecycleManager,
        workspace_lock: WorkspaceLockManager,
        cancellation: CancellationManager | None = None,
        console: Console | None = None,
        recovery_report: RecoveryReport | None = None,
        health_report: ComponentHealthReport | None = None,
    ) -> None:
        self.context = context
        self.graph = graph
        self.lifecycle = lifecycle
        self.workspace_lock = workspace_lock
        self.cancellation = cancellation or CancellationManager()
        self.console = console or Console()
        self.recovery_report = recovery_report
        self.health_report = health_report
        self.recovery_gate_decision: RecoveryGateDecision | None = getattr(
            graph,
            "recovery_gate_decision",
            None,
        )
        self.shutdown_summary: ShutdownSummary | None = None
        self._final_report: FinalReport | None = None
        self._interaction_report: InteractionFinalReport | None = None
        self._finalizing_during_shutdown = False
        self._resources_closed = False
        interaction_controller = getattr(self.graph, "interaction_controller", None)
        if interaction_controller is None:
            interaction_controller = InteractionController(
                trace=getattr(self.graph, "trace", None),
                cancellation_manager=self.cancellation,
            )
            with suppress(Exception):
                self.graph.interaction_controller = interaction_controller
        self.interaction_controller: InteractionController = interaction_controller
        self.interaction_controller.cancellation_manager = self.cancellation
        self.graph.install_cancellation_tokens(self.cancellation.child_token)

    def boot(self) -> AgentKernel:
        self.context.status = KernelStatus.READY
        return self

    def run_task(self, user_goal: str) -> RunResult:
        self.context.status = KernelStatus.RUNNING
        self.lifecycle.start_task(user_goal)
        try:
            self.cancellation.throw_if_cancelled()
            if (
                self.recovery_gate_decision is not None
                and not self.recovery_gate_decision.can_call_model
            ):
                return self._blocked_by_recovery_gate()
            agent = AgentLoop(
                model_runner=self.graph.model_runner,
                tools=self.graph.tools,
                trace=self.graph.trace,
                console=self.console,
                max_turns=self.graph.config.max_turns,
                planner=self.graph.planner,
                tool_executor=self.graph.tool_executor,
                tool_protocol=self.graph.tool_protocol,
                prompt_assembly=self.graph.prompt_assembly,
                interaction_controller=self.interaction_controller,
                context_manager=getattr(self.graph, "context_manager", None),
                context_db_path=self.graph.config.context_db_path(self.graph.trace.store.run_dir),
                strict=self.graph.config.strict,
            )
            agent_result = agent.run(user_goal)
            final_answer = str(agent_result.final_answer)
            self.graph.workspace_state.record_external_changes()
            if agent_result.status == AgentLoopStatus.COMPLETED:
                self.lifecycle.mark_completed(final_answer)
                shutdown_reason = ShutdownReason.NORMAL
                result_status = RunStatus.COMPLETED
            elif agent_result.status == AgentLoopStatus.BLOCKED:
                self.lifecycle.mark_blocked(
                    f"{agent_result.status.value}: {agent_result.error_code or final_answer}"
                )
                self.context.diagnostics.append(
                    {
                        "type": "AgentLoopStatus",
                        "status": agent_result.status.value,
                        "error_code": agent_result.error_code,
                        "message": final_answer,
                    }
                )
                shutdown_reason = ShutdownReason.BLOCKED
                result_status = RunStatus.BLOCKED
            else:
                self.lifecycle.mark_failed(
                    f"{agent_result.status.value}: {agent_result.error_code or final_answer}"
                )
                self.context.diagnostics.append(
                    {
                        "type": "AgentLoopStatus",
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
            return RunResult(
                final_answer=final_answer,
                final_report=report,
                status=result_status,
                interaction_report=interaction_report,
            )
        except KeyboardInterrupt:
            self.interaction_controller.handle_command(
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
            raise CancellationError("Cancelled by KeyboardInterrupt.", code="keyboard_interrupt") from None
        except CancellationError:
            self.interaction_controller.handle_command(
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

    def _blocked_by_recovery_gate(self) -> RunResult:
        decision = self.recovery_gate_decision
        assert decision is not None
        message = (
            "Session recovery requires review before the model can continue: "
            + ", ".join(decision.blockers)
        )
        self.lifecycle.mark_blocked(message)
        self.context.diagnostics.append(
            {
                "type": "SessionRecoveryGate",
                "status": decision.status.value,
                "blockers": list(decision.blockers),
                "next_action": decision.next_action,
            }
        )
        if self.graph.planner.state is not None:
            if decision.status == RecoveryGateStatus.BLOCKED:
                self.graph.planner.abort("session recovery blocked")
            else:
                self.graph.planner.interrupt("session recovery needs review")
        self.graph.trace.record(
            "session.recovery_blocked",
            {
                "run_id": self.context.identity.run_id,
                "session_id": self.context.identity.session_id,
                "task_id": self.context.identity.task_id,
                **decision.to_dict(),
            },
        )
        self.shutdown(ShutdownReason.BLOCKED)
        report = self.final_report()
        return RunResult(
            final_answer=message,
            final_report=report,
            status=RunStatus.BLOCKED,
            interaction_report=self.interaction_final_report(),
        )

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
            model=self.graph.model_runner,
            command=self.graph.command_executor,
            sandbox=self.graph.sandbox_manager,
            mutation=self.graph.mutation_manager,
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

    def close_resources(self) -> None:
        if self._resources_closed:
            return
        shutdown_reason = self.shutdown_summary.reason if self.shutdown_summary else None
        session_status = "closed" if shutdown_reason == ShutdownReason.NORMAL else "interrupted"
        for name, component in (
            ("workspace_state", self.graph.workspace_state),
            ("context_manager", self.graph.context_manager),
            ("tool_protocol", self.graph.tool_protocol),
        ):
            try:
                if name == "workspace_state" and hasattr(component, "close_session"):
                    component.close_session(status=session_status)
                close = getattr(component, "close", None)
                if callable(close):
                    close()
            except Exception as exc:
                self.context.diagnostics.append(
                    {
                        "type": type(exc).__name__,
                        "message": str(exc),
                        "stage": f"{name}_close",
                    }
                )
        self._resources_closed = True

    def recover_previous_run(self) -> RecoveryReport:
        report = CrashRecoveryManager(
            trace=self.graph.trace,
            workspace_lock=self.workspace_lock,
            workspace_state=self.graph.workspace_state,
            sandbox=self.graph.sandbox_manager,
            command=self.graph.command_executor,
        ).recover()
        self.recovery_report = report
        self.context.recovered_previous_run = report.recovered
        return report

    def health_check(self) -> ComponentHealthReport:
        checker = ComponentHealthChecker(trace=self.graph.trace)
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
            component_health_summary=(self.health_report.to_dict() if self.health_report else {}),
            shutdown_summary=self.shutdown_summary,
            recovery_summary=(self.recovery_report.to_dict() if self.recovery_report else {}),
            lifecycle_summary=self.lifecycle.summary(),
            config_summary=self.graph.config.final_report_config_summary(),
            workspace_summary=workspace_health.to_dict(),
            trace_summary=trace_summary,
            session_summary={
                "session_id": self.context.identity.session_id,
                "task_id": self.context.identity.task_id,
                "run_id": self.context.identity.run_id,
                "run_mode": self.context.session_run_mode,
            },
            checkpoint_summary={
                "workspace_health": workspace_health.to_dict(),
                "last_safe_checkpoint": (
                    self.recovery_gate_decision.resume_context.workspace
                    if self.recovery_gate_decision is not None
                    else {}
                ),
            },
            recovery_gate_summary=(
                self.recovery_gate_decision.to_dict()
                if self.recovery_gate_decision is not None
                else {}
            ),
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
        memory_pipeline = getattr(self.graph, "memory_pipeline", None)
        if memory_pipeline is None or not hasattr(memory_pipeline, "ingest_session_end"):
            return
        try:
            memory_pipeline.ingest_session_end(
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
        interaction_report = self.interaction_controller.build_final_report(
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
