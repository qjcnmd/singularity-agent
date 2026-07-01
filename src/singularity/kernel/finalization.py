from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.kernel.models import KernelContext
from singularity.observability.redaction import TraceRedactor


@dataclass(frozen=True)
class FinalReport:
    run_id: str
    session_id: str
    task_id: str
    kernel_status: str
    shutdown_reason: str | None
    diagnostics_count: int
    cleanup_status: str
    recovered_previous_run: bool
    uncertain_transactions: list[str]
    workspace_lock_status: str
    planner_summary: dict[str, Any] = field(default_factory=dict)
    verification_summary: dict[str, Any] = field(default_factory=dict)
    policy_summary: dict[str, Any] = field(default_factory=dict)
    sandbox_summary: dict[str, Any] = field(default_factory=dict)
    model_summary: dict[str, Any] = field(default_factory=dict)
    trace_summary: dict[str, Any] = field(default_factory=dict)
    config_summary: dict[str, Any] = field(default_factory=dict)
    workspace_summary: dict[str, Any] = field(default_factory=dict)
    component_health_summary: dict[str, Any] = field(default_factory=dict)
    shutdown_summary: dict[str, Any] = field(default_factory=dict)
    recovery_summary: dict[str, Any] = field(default_factory=dict)
    session_summary: dict[str, Any] = field(default_factory=dict)
    checkpoint_summary: dict[str, Any] = field(default_factory=dict)
    recovery_gate_summary: dict[str, Any] = field(default_factory=dict)
    lifecycle_summary: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "kernel_status": self.kernel_status,
            "shutdown_reason": self.shutdown_reason,
            "diagnostics_count": self.diagnostics_count,
            "cleanup_status": self.cleanup_status,
            "recovered_previous_run": self.recovered_previous_run,
            "uncertain_transactions": self.uncertain_transactions,
            "workspace_lock_status": self.workspace_lock_status,
            "planner_summary": self.planner_summary,
            "verification_summary": self.verification_summary,
            "policy_summary": self.policy_summary,
            "sandbox_summary": self.sandbox_summary,
            "model_summary": self.model_summary,
            "trace_summary": self.trace_summary,
            "config_summary": self.config_summary,
            "workspace_summary": self.workspace_summary,
            "component_health_summary": self.component_health_summary,
            "shutdown_summary": self.shutdown_summary,
            "recovery_summary": self.recovery_summary,
            "session_summary": self.session_summary,
            "checkpoint_summary": self.checkpoint_summary,
            "recovery_gate_summary": self.recovery_gate_summary,
            "lifecycle_summary": self.lifecycle_summary,
        }


PartialFinalReport = FinalReport


class KernelFinalizer:
    def __init__(self) -> None:
        self.redactor = TraceRedactor()

    def finalize(
        self,
        *,
        context: KernelContext,
        planner_report: Any | None = None,
        component_health_summary: dict[str, Any] | None = None,
        shutdown_summary: Any | None = None,
        recovery_summary: dict[str, Any] | None = None,
        lifecycle_summary: dict[str, Any] | None = None,
        config_summary: dict[str, Any] | None = None,
        workspace_summary: dict[str, Any] | None = None,
        trace_summary: dict[str, Any] | None = None,
        session_summary: dict[str, Any] | None = None,
        checkpoint_summary: dict[str, Any] | None = None,
        recovery_gate_summary: dict[str, Any] | None = None,
    ) -> FinalReport:
        context.status = type(context.status).FINALIZED
        planner_payload = _to_dict(planner_report)
        shutdown_payload = _to_dict(shutdown_summary)
        report = FinalReport(
            run_id=context.identity.run_id,
            session_id=context.identity.session_id,
            task_id=context.identity.task_id,
            kernel_status=context.status.value,
            shutdown_reason=shutdown_payload.get("reason"),
            diagnostics_count=len(context.diagnostics),
            cleanup_status=str(shutdown_payload.get("cleanup_status") or "not_started"),
            recovered_previous_run=context.recovered_previous_run,
            uncertain_transactions=list(context.uncertain_transactions),
            workspace_lock_status=context.workspace_lock_status,
            planner_summary=planner_payload,
            verification_summary=dict(planner_payload.get("verification_summary") or {}),
            policy_summary=dict(planner_payload.get("policy_approval_summary") or {}),
            sandbox_summary=dict(planner_payload.get("sandbox_isolation_summary") or {}),
            model_summary=dict(planner_payload.get("model_usage_summary") or {}),
            trace_summary=trace_summary or dict(planner_payload.get("execution_trace_summary") or {}),
            config_summary=config_summary or {},
            workspace_summary=workspace_summary or {},
            component_health_summary=component_health_summary or {},
            shutdown_summary=shutdown_payload,
            recovery_summary=recovery_summary or {},
            session_summary=session_summary or {},
            checkpoint_summary=checkpoint_summary or {},
            recovery_gate_summary=recovery_gate_summary or {},
            lifecycle_summary=lifecycle_summary or {},
        )
        redacted = self.redactor.redact_payload(report.to_dict())
        return FinalReport(**redacted)


def _to_dict(value: Any | None) -> dict[str, Any]:
    if value is None:
        return {}
    if hasattr(value, "to_dict"):
        payload = value.to_dict()
        return payload if isinstance(payload, dict) else {}
    if isinstance(value, dict):
        return value
    return {"value": str(value)}
