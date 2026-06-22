from __future__ import annotations

from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.config import ProductionRuntimeConfig
from singularity.interaction import (
    InteractionMode,
    InteractionRuntime,
    RichCliRenderer,
    RichInteractionProvider,
)
from singularity.observability import TraceRuntime
from singularity.policy import ApprovalMode
from singularity.workspace_state import LocalWorkspaceStateRuntime

from singularity.kernel.cancellation import CancellationManager
from singularity.kernel.exceptions import KernelBootstrapError
from singularity.kernel.finalization import KernelFinalizer
from singularity.kernel.graph import RuntimeFactory
from singularity.kernel.health import RuntimeHealthChecker
from singularity.kernel.lifecycle import RunLifecycleManager
from singularity.kernel.locks import WorkspaceLockManager
from singularity.kernel.models import (
    KernelContext,
    KernelStatus,
    RunIdentity,
    ShutdownReason,
)
from singularity.kernel.recovery import CrashRecoveryManager
from singularity.kernel.runtime import AgentKernel
from singularity.kernel.shutdown import ShutdownSummary


class KernelBootstrap:
    def __init__(
        self,
        *,
        project_root: Path | str,
        config: ProductionRuntimeConfig | None = None,
        trace: TraceRuntime | None = None,
        console: Console | None = None,
        runtime_factory: RuntimeFactory | None = None,
        workspace_lock: WorkspaceLockManager | None = None,
    ) -> None:
        self.project_root = Path(project_root).expanduser().resolve(strict=False)
        self.config = config
        self.trace = trace
        self.console = console or Console()
        self.runtime_factory = runtime_factory or RuntimeFactory()
        self.workspace_lock = workspace_lock or WorkspaceLockManager(self.project_root)

    def boot(self, user_goal: str) -> AgentKernel:
        config = self.config or ProductionRuntimeConfig.from_cli(project_root=self.project_root)
        trace = self.trace or TraceRuntime.create(
            self.project_root,
            run_id=config.resume_session,
            session_id=config.resume_session,
            trace_dir=config.trace_dir,
        )
        identity = RunIdentity.new(
            run_id=trace.run_id,
            session_id=config.resume_session or trace.session_id,
            task_id=trace.run_id,
        )
        interaction_mode = (
            InteractionMode.NON_INTERACTIVE
            if config.approval_mode == ApprovalMode.NON_INTERACTIVE
            else config.interaction_mode
        )
        renderer = RichCliRenderer(self.console)
        provider = (
            RichInteractionProvider(self.console)
            if interaction_mode == InteractionMode.INTERACTIVE
            else None
        )
        cancellation = CancellationManager()
        interaction_runtime = InteractionRuntime(
            mode=interaction_mode,
            trace=trace,
            provider=provider,
            sinks=[renderer],
            cancellation_manager=cancellation,
        )
        if hasattr(trace, "set_interaction_sink"):
            trace.set_interaction_sink(interaction_runtime.consume_trace_event)
        lifecycle = RunLifecycleManager(identity=identity, trace=trace)
        run = lifecycle.create_run(user_goal)
        session = lifecycle.start_session()
        context = KernelContext(
            project_root=self.project_root,
            identity=identity,
            run=run,
            session=session,
            status=KernelStatus.BOOTING,
        )
        trace.record("kernel.boot.started", {"run_id": identity.run_id, "session_id": identity.session_id})
        trace.emit(
            "context.observation_added",
            runtime="config",
            summary="Effective runtime config resolved.",
            payload=config.effective_config(),
            ids={"run_id": identity.run_id, "session_id": identity.session_id},
        )
        try:
            self.workspace_lock.acquire_lock(
                run_id=identity.run_id,
                read_only=config.approval_mode == ApprovalMode.READ_ONLY,
            )
            context.workspace_lock_status = "acquired"
            recovery_workspace_state = LocalWorkspaceStateRuntime(self.project_root, trace=trace)
            try:
                recovery = CrashRecoveryManager(
                    trace=trace,
                    workspace_lock=self.workspace_lock,
                    workspace_state=recovery_workspace_state,
                ).recover()
            finally:
                recovery_workspace_state.close()
            context.recovered_previous_run = recovery.recovered
            graph = self.runtime_factory.build(
                project_root=self.project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                interaction_runtime=interaction_runtime,
            )
            context.components = dict(graph.components)
            health = RuntimeHealthChecker(trace=trace).enforce(graph.components_for_health())
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
                details={"error_type": type(exc).__name__, "message": str(exc)},
                final_report=final_report,
            ) from exc


def build_config_from_cli(**kwargs: Any) -> ProductionRuntimeConfig:
    return ProductionRuntimeConfig.from_cli(**kwargs)
