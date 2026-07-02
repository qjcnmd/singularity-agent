from __future__ import annotations

from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.config import ProductionConfig
from singularity.context import RecoveryManager
from singularity.context.models import RecoveredContext
from singularity.interaction import (
    InteractionController,
    InteractionMode,
    RichCliRenderer,
    RichInteractionProvider,
)
from singularity.kernel.agent_kernel import AgentKernel
from singularity.kernel.cancellation import CancellationManager
from singularity.kernel.exceptions import KernelBootstrapError
from singularity.kernel.finalization import KernelFinalizer
from singularity.kernel.graph import AgentGraphBuilder
from singularity.kernel.health import ComponentHealthChecker
from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.locks import WorkspaceLockManager
from singularity.kernel.models import (
    KernelContext,
    KernelStatus,
    RunIdentity,
    ShutdownReason,
)
from singularity.kernel.recovery import CrashRecoveryManager
from singularity.kernel.shutdown import ShutdownSummary
from singularity.observability import TraceRecorder
from singularity.policy import PermissionProfileName
from singularity.session.history import SessionHistoryReader
from singularity.session.models import SessionRunMode, SessionStatus
from singularity.session.recovery import SessionRecoveryGate
from singularity.session.store import SessionStore
from singularity.workspace_state import WorkspaceStateManager


class KernelBootstrap:
    def __init__(
        self,
        *,
        project_root: Path | str,
        config: ProductionConfig | None = None,
        trace: TraceRecorder | None = None,
        console: Console | None = None,
        component_factory: AgentGraphBuilder | None = None,
        workspace_lock: WorkspaceLockManager | None = None,
    ) -> None:
        self.project_root = Path(project_root).expanduser().resolve(strict=False)
        self.config = config
        self.trace = trace
        self.console = console or Console()
        self.component_factory = component_factory or AgentGraphBuilder()
        self.workspace_lock = workspace_lock or WorkspaceLockManager(self.project_root)

    def boot(self, user_goal: str) -> AgentKernel:
        config = self.config or ProductionConfig.from_cli(project_root=self.project_root)
        session_store = SessionStore(self.project_root)
        try:
            launch = session_store.prepare_launch(
                mode=config.session_run_mode,
                requested_session_id=config.resume_session,
                user_goal=user_goal,
                project_root=self.project_root,
            )
        finally:
            session_store.close()
        trace = self.trace or TraceRecorder.create(
            self.project_root,
            run_id=launch.run_id,
            session_id=launch.session_id,
            trace_dir=config.trace_dir,
        )
        identity = RunIdentity.new(
            run_id=trace.run_id,
            session_id=launch.session_id,
            task_id=launch.task_id,
        )
        interaction_mode = config.interaction_mode
        renderer = RichCliRenderer(self.console)
        provider = (
            RichInteractionProvider(self.console)
            if interaction_mode == InteractionMode.INTERACTIVE
            else None
        )
        cancellation = CancellationManager()
        interaction_controller = InteractionController(
            mode=interaction_mode,
            trace=trace,
            provider=provider,
            sinks=[renderer],
            cancellation_manager=cancellation,
        )
        if hasattr(trace, "set_interaction_sink"):
            trace.set_interaction_sink(interaction_controller.consume_trace_event)
        lifecycle = RunLifecycleManager(identity=identity, trace=trace)
        run = lifecycle.create_run(user_goal)
        session = lifecycle.start_session()
        context = KernelContext(
            project_root=self.project_root,
            identity=identity,
            run=run,
            session=session,
            status=KernelStatus.BOOTING,
            session_run_mode=launch.mode.value,
        )
        self._persist_launch(
            launch=launch,
            project_root=self.project_root,
            trace_run_dir=trace.store.run_dir,
        )
        trace.record(
            "session.created" if launch.mode == SessionRunMode.NEW else f"session.{launch.mode.value}_requested",
            {"run_id": identity.run_id, **launch.to_dict()},
        )
        trace.record("kernel.boot.started", {"run_id": identity.run_id, "session_id": identity.session_id})
        trace.emit(
            "context.observation_added",
            component="config",
            summary="Effective component config resolved.",
            payload=config.effective_config(),
            ids={"run_id": identity.run_id, "session_id": identity.session_id},
        )
        try:
            self.workspace_lock.acquire_lock(
                run_id=identity.run_id,
                read_only=config.permission_profile == PermissionProfileName.READ_ONLY,
            )
            context.workspace_lock_status = "acquired"
            recovery_workspace_state = WorkspaceStateManager(self.project_root, trace=trace)
            try:
                if launch.mode == SessionRunMode.NEW:
                    recovery_workspace_state.begin_session(
                        task_id=identity.task_id,
                        session_id=identity.session_id,
                    )
                else:
                    recovery_workspace_state.recover_session(identity.session_id)
                recovery = CrashRecoveryManager(
                    trace=trace,
                    workspace_lock=self.workspace_lock,
                    workspace_state=recovery_workspace_state,
                ).inspect(session_id=identity.session_id)
                recovery = recovery.with_stale_lock(
                    recovery.stale_lock_detected or self.workspace_lock.last_stale_lock_detected
                )
                workspace_health = recovery_workspace_state.get_workspace_health()
                history_reader = SessionHistoryReader(self.project_root)
                previous_tool_protocol_path = self._previous_tool_protocol_path(
                    launch.previous_run_id,
                    launch.previous_trace_run_dir,
                    config.trace_dir,
                )
                context_recovery = self._previous_context_recovery(
                    launch.previous_run_id,
                    launch.previous_trace_run_dir,
                    config.trace_dir,
                )
                tool_protocol_report = history_reader.tool_protocol_report(
                    run_id=launch.previous_run_id or identity.run_id,
                    session_id=identity.session_id,
                    task_id=identity.task_id,
                    state_path=previous_tool_protocol_path,
                )
                planner_state = history_reader.planner_state(identity.session_id)
                resume_context = history_reader.build_resume_context(
                    session_id=identity.session_id,
                    user_goal=user_goal,
                    workspace_health=workspace_health,
                    current_run_id=identity.run_id,
                    task_id=identity.task_id,
                    trace=trace,
                    tool_protocol_state_path=previous_tool_protocol_path,
                    tool_protocol_run_id=launch.previous_run_id or identity.run_id,
                )
                trace.record(
                    "session.recovery_gate_started",
                    {
                        "run_id": identity.run_id,
                        "session_id": identity.session_id,
                        "task_id": identity.task_id,
                        "mode": launch.mode.value,
                    },
                )
                recovery_gate_decision = SessionRecoveryGate().evaluate(
                    session_id=identity.session_id,
                    mode=launch.mode.value,
                    workspace_health=workspace_health,
                    crash_recovery=recovery,
                    tool_protocol_report=tool_protocol_report,
                    context_recovery=context_recovery,
                    planner_state=planner_state,
                    resume_context=resume_context,
                )
                context.recovery_gate_decision = recovery_gate_decision.to_dict()
                trace.record(
                    "session.recovery_gate_completed",
                    {
                        "run_id": identity.run_id,
                        "session_id": identity.session_id,
                        "task_id": identity.task_id,
                        **recovery_gate_decision.to_dict(),
                    },
                )
            finally:
                recovery_workspace_state.close()
            context.recovered_previous_run = recovery.recovered
            graph = self.component_factory.build(
                project_root=self.project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                workspace_health=workspace_health,
                recovery_gate_decision=recovery_gate_decision,
                interaction_controller=interaction_controller,
            )
            context.components = dict(graph.components)
            health = ComponentHealthChecker(trace=trace).enforce(graph.components_for_health())
            kernel = AgentKernel(
                context=context,
                graph=graph,
                lifecycle=lifecycle,
                workspace_lock=self.workspace_lock,
                cancellation=cancellation,
                console=self.console,
                recovery_report=recovery,
                health_report=health,
            ).boot()
            kernel.recovery_gate_decision = recovery_gate_decision
            trace.record("kernel.boot.completed", {"run_id": identity.run_id})
            return kernel
        except Exception as exc:
            context.status = KernelStatus.FAILED
            context.diagnostics.append({"type": type(exc).__name__, "message": str(exc)})
            lifecycle.mark_failed(exc)
            trace.record(
                "kernel.boot.failed",
                {"type": type(exc).__name__, "message": str(exc)},
            )
            self._finish_launch_failure(
                run_id=identity.run_id,
                session_id=identity.session_id,
                task_id=identity.task_id,
                error=exc,
            )
            self.workspace_lock.release_lock()
            context.workspace_lock_status = "released"
            final_report = KernelFinalizer().finalize(
                context=context,
                shutdown_summary=ShutdownSummary(
                    ShutdownReason.BOOTSTRAP_FAILED,
                    "completed",
                    [{"step": "release_lock", "status": "completed"}],
                ),
                lifecycle_summary=lifecycle.summary(),
                config_summary=config.final_report_config_summary(),
            )
            trace.record("finalization.completed", final_report.to_dict())
            if hasattr(trace, "store"):
                trace.store._write_index()
            raise KernelBootstrapError(
                "Kernel bootstrap failed.",
                code="kernel_bootstrap_failed",
                details={
                    "error_type": type(exc).__name__,
                    "message": str(exc),
                    "session_id": identity.session_id,
                    "run_id": identity.run_id,
                    "task_id": identity.task_id,
                },
                final_report=final_report,
            ) from exc

    def _previous_tool_protocol_path(
        self,
        previous_run_id: str | None,
        previous_trace_run_dir: str | None,
        trace_dir: Path | None,
    ) -> Path | None:
        if not previous_run_id:
            return None
        if previous_trace_run_dir:
            return Path(previous_trace_run_dir) / "tool_protocol.sqlite3"
        if trace_dir is not None:
            return Path(trace_dir) / previous_run_id / "tool_protocol.sqlite3"
        return self.project_root / "work" / "traces" / "runs" / previous_run_id / "tool_protocol.sqlite3"

    def _previous_context_recovery(
        self,
        previous_run_id: str | None,
        previous_trace_run_dir: str | None,
        trace_dir: Path | None,
    ) -> dict[str, Any] | None:
        context_db_path = self._previous_context_db_path(
            previous_run_id,
            previous_trace_run_dir,
            trace_dir,
        )
        if previous_run_id is None or context_db_path is None or not context_db_path.exists():
            return None
        trace_path = context_db_path.parent / "events.jsonl"
        manager = None
        try:
            manager = RecoveryManager(
                context_db_path,
                trace_path=trace_path if trace_path.exists() else None,
            )
            recovered = manager.recover(previous_run_id)
        except Exception as exc:
            return {
                "recommended_next_action": "needs_review",
                "context_recovery_failed": True,
                "recovery_warnings": [f"context recovery inspect failed: {type(exc).__name__}"],
            }
        finally:
            if manager is not None:
                manager.store.close()
        return _context_recovery_summary(recovered)

    def _previous_context_db_path(
        self,
        previous_run_id: str | None,
        previous_trace_run_dir: str | None,
        trace_dir: Path | None,
    ) -> Path | None:
        if not previous_run_id:
            return None
        if previous_trace_run_dir:
            return Path(previous_trace_run_dir) / "context.sqlite3"
        if trace_dir is not None:
            return Path(trace_dir) / previous_run_id / "context.sqlite3"
        return self.project_root / "work" / "traces" / "runs" / previous_run_id / "context.sqlite3"

    def _persist_launch(
        self,
        *,
        launch: Any,
        project_root: Path,
        trace_run_dir: Path,
    ) -> None:
        store = SessionStore(self.project_root)
        try:
            store.create_session(
                session_id=launch.session_id,
                project_root=project_root,
                user_goal=launch.user_goal,
                task_id=launch.task_id,
            )
            store.start_run(
                session_id=launch.session_id,
                run_id=launch.run_id,
                task_id=launch.task_id,
                mode=launch.mode,
                user_goal=launch.user_goal,
                trace_run_dir=trace_run_dir,
            )
            event_type = {
                SessionRunMode.NEW: "session.created",
                SessionRunMode.CONTINUE: "session.continue_requested",
                SessionRunMode.RESUME: "session.resume_requested",
            }[launch.mode]
            store.append_timeline_event(
                session_id=launch.session_id,
                run_id=launch.run_id,
                task_id=launch.task_id,
                event_type=event_type,
                summary=f"Session {launch.mode.value} launch requested.",
                payload={**launch.to_dict(), "trace_run_dir": str(trace_run_dir)},
            )
        finally:
            store.close()

    def _finish_launch_failure(
        self,
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        error: BaseException,
    ) -> None:
        store = SessionStore(self.project_root)
        try:
            store.finish_run(
                run_id=run_id,
                status=SessionStatus.FAILED,
                summary={"error_type": type(error).__name__, "message": str(error)},
            )
            store.append_timeline_event(
                session_id=session_id,
                run_id=run_id,
                task_id=task_id,
                event_type="session.run_failed",
                summary=f"Kernel bootstrap failed with {type(error).__name__}.",
                payload={"message": str(error)},
            )
        finally:
            store.close()


def _context_recovery_summary(recovered: RecoveredContext) -> dict[str, Any]:
    return {
        "run_id": recovered.run_id,
        "pending_tool_calls": [
            {"id": call.get("id"), "name": (call.get("function") or {}).get("name")}
            for call in recovered.pending_tool_calls
        ],
        "pending_policy_approval": recovered.pending_policy_approval,
        "active_process_sessions": recovered.active_process_sessions,
        "open_mutation_transactions": recovered.open_mutation_transactions,
        "last_verification_status": recovered.last_verification_status,
        "last_safe_checkpoint": recovered.last_safe_checkpoint,
        "recommended_next_action": recovered.recommended_next_action,
        "recovery_warnings": recovered.recovery_warnings,
        "trace_last_event": recovered.trace_last_event,
    }
