from __future__ import annotations

from collections.abc import Callable
from dataclasses import replace
from pathlib import Path
from typing import Any

from singularity.agent_host.models import (
    ApprovalEvent,
    HostedRunResult,
    RunEvent,
    RunSession,
    RunStateSnapshot,
)
from singularity.config import ProductionConfig
from singularity.kernel import KernelBootstrap
from singularity.kernel.models import CancellationReason, RunIdentity
from singularity.observability.artifacts import TraceArtifactStore
from singularity.observability.store import TraceStore
from singularity.policy import ApprovalGrant
from singularity.runtime.resources import close_runtime_resources


class AgentHostError(RuntimeError):
    pass


class AgentHost:
    def __init__(
        self,
        project_root: Path | str,
        *,
        bootstrap_factory: Callable[..., Any] = KernelBootstrap,
    ) -> None:
        self.project_root = Path(project_root).expanduser().resolve(strict=False)
        self.bootstrap_factory = bootstrap_factory
        self._sessions: dict[str, RunSession] = {}
        self._kernels: dict[str, Any] = {}

    def start_run(
        self,
        goal: str,
        *,
        config: ProductionConfig | None = None,
    ) -> HostedRunResult:
        resolved_config = config or ProductionConfig.from_cli(project_root=self.project_root)
        kernel = self.bootstrap_factory(
            project_root=self.project_root,
            config=resolved_config,
        ).boot(goal)
        identity = kernel.context.identity
        trace_run_dir = Path(kernel.graph.trace.store.run_dir)
        session = RunSession(
            run_id=identity.run_id,
            session_id=identity.session_id,
            task_id=identity.task_id,
            status="running",
            trace_run_dir=trace_run_dir,
        )
        self._sessions[identity.run_id] = session
        self._kernels[identity.run_id] = kernel
        try:
            result = kernel.run_task(goal)
            session.status = _value(getattr(result, "status", "completed"))
            session.final_answer = str(getattr(result, "final_answer", ""))
            session.final_report = _to_dict(getattr(result, "final_report", None))
            snapshot = self.snapshot(identity.run_id)
            return HostedRunResult(
                run_id=identity.run_id,
                session_id=identity.session_id,
                task_id=identity.task_id,
                status=session.status,
                final_answer=session.final_answer,
                final_report=session.final_report or {},
                snapshot=snapshot,
            )
        finally:
            close_runtime_resources(kernel)

    def resume_run(
        self,
        session_id: str,
        goal: str,
        *,
        config: ProductionConfig | None = None,
    ) -> HostedRunResult:
        resolved_config = (
            replace(config, resume_session=session_id, session_run_mode="resume")
            if config is not None
            else ProductionConfig.from_cli(
                project_root=self.project_root,
                resume_session=session_id,
                session_run_mode="resume",
            )
        )
        return self.start_run(goal, config=resolved_config)

    def cancel_run(self, run_id: str, *, message: str = "cancelled by AgentHost") -> RunStateSnapshot:
        kernel = self._kernels.get(run_id)
        if kernel is None:
            raise AgentHostError(f"Unknown active run: {run_id}")
        kernel.cancel(CancellationReason.USER_INTERRUPTED, message)
        session = self._sessions.get(run_id)
        if session is not None:
            session.status = "cancel_requested"
        return self.snapshot(run_id)

    def submit_approval(self, run_id: str, grant: ApprovalGrant | dict[str, Any]) -> ApprovalEvent:
        kernel = self._kernels.get(run_id)
        if kernel is None:
            raise AgentHostError(f"Unknown active run: {run_id}")
        approval_grant = ApprovalGrant.from_dict(grant) if isinstance(grant, dict) else grant
        approval_gate = getattr(kernel.graph, "approval_gate", None)
        if approval_gate is None or not hasattr(approval_gate, "register_grant"):
            raise AgentHostError("Active run does not expose ApprovalGate.register_grant.")
        approval_gate.register_grant(approval_grant)
        event = ApprovalEvent.from_grant(approval_grant)
        trace = getattr(kernel.graph, "trace", None)
        if trace is not None and hasattr(trace, "emit"):
            trace.emit(
                "approval.granted",
                component="agent_host",
                summary=event.reason or "Approval grant registered.",
                payload=event.to_dict(),
                ids={
                    "run_id": run_id,
                    "session_id": event.session_id,
                    "approval_grant_id": event.grant_id,
                    "policy_decision_id": event.decision_id,
                },
            )
        return event

    def snapshot(self, run_id: str) -> RunStateSnapshot:
        store = self._trace_store(run_id)
        events = store.query_events(run_id=run_id)
        artifacts = store.artifacts()
        session = self._sessions.get(run_id)
        last_sequence = len(events) - 1 if events else None
        if session is not None:
            return session.to_snapshot(
                event_count=len(events),
                artifact_count=len(artifacts),
                last_sequence=last_sequence,
            )
        identity = _identity_from_events(run_id, events)
        return RunStateSnapshot(
            run_id=identity.run_id,
            session_id=identity.session_id,
            task_id=identity.task_id,
            status="unknown",
            trace_run_dir=str(store.run_dir),
            event_count=len(events),
            artifact_count=len(artifacts),
            last_sequence=last_sequence,
        )

    def events(self, run_id: str, *, after_sequence: int | None = None) -> list[RunEvent]:
        store = self._trace_store(run_id)
        projected = [
            RunEvent.from_trace_event(event, sequence=sequence)
            for sequence, event in enumerate(store.query_events(run_id=run_id))
        ]
        if after_sequence is None:
            return projected
        return [event for event in projected if event.sequence > after_sequence]

    def read_artifact(self, run_id: str, artifact_ref: str) -> bytes:
        store = self._trace_store(run_id)
        artifact = next((item for item in store.artifacts() if item.artifact_id == artifact_ref), None)
        session_id = artifact.session_id if artifact is not None else run_id
        return TraceArtifactStore(
            self.project_root,
            run_id=run_id,
            session_id=session_id,
            run_dir=store.run_dir,
        ).read_artifact(artifact_ref)

    def _trace_store(self, run_id: str) -> TraceStore:
        kernel = self._kernels.get(run_id)
        if kernel is not None:
            trace = getattr(kernel.graph, "trace", None)
            store = getattr(trace, "store", None)
            if store is not None:
                return store
        session = self._sessions.get(run_id)
        trace_dir = session.trace_run_dir.parent if session is not None else None
        return TraceStore(self.project_root, run_id=run_id, trace_dir=trace_dir)


def _identity_from_events(run_id: str, events: list[Any]) -> RunIdentity:
    if not events:
        return RunIdentity(run_id=run_id, session_id=None or run_id, task_id=None or run_id)
    first = events[0]
    return RunIdentity(
        run_id=run_id,
        session_id=first.session_id or run_id,
        task_id=first.task_id or run_id,
    )


def _to_dict(value: Any | None) -> dict[str, Any]:
    if value is None:
        return {}
    if hasattr(value, "to_dict"):
        payload = value.to_dict()
        return payload if isinstance(payload, dict) else {}
    return value if isinstance(value, dict) else {"value": str(value)}


def _value(value: Any) -> str:
    return value.value if hasattr(value, "value") else str(value)
