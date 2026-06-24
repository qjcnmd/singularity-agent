from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.kernel.exceptions import ComponentHealthError
from singularity.kernel.models import ComponentName


DEFAULT_HEALTH_COMPONENTS = [
    ComponentName.CONFIGURATION,
    ComponentName.OBSERVABILITY,
    ComponentName.INTERACTION,
    ComponentName.WORKSPACE_STATE,
    ComponentName.PROJECT_INDEX,
    ComponentName.MEMORY,
    ComponentName.POLICY,
    ComponentName.SANDBOX,
    ComponentName.COMMAND,
    ComponentName.MUTATION,
    ComponentName.EDIT,
    ComponentName.TOOLS,
    ComponentName.TOOL_EXECUTOR,
    ComponentName.TOOL_PROTOCOL,
    ComponentName.VERIFICATION,
    ComponentName.REVIEW,
    ComponentName.EVALUATION,
    ComponentName.INSTRUCTIONS,
    ComponentName.MODEL,
    ComponentName.CONTEXT,
    ComponentName.PLANNER,
]


@dataclass(frozen=True)
class ComponentHealthReport:
    ok: bool
    summary: dict[str, str]
    diagnostics: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "summary": self.summary,
            "diagnostics": self.diagnostics,
        }


class ComponentHealthChecker:
    def __init__(
        self,
        *,
        trace: Any | None = None,
        critical_components: set[ComponentName] | None = None,
    ) -> None:
        self.trace = trace
        self.critical_components = (
            critical_components
            if critical_components is not None
            else set(DEFAULT_HEALTH_COMPONENTS)
        )

    def check(self, components: dict[str | ComponentName, Any]) -> ComponentHealthReport:
        normalized = {
            key.value if isinstance(key, ComponentName) else str(key): value
            for key, value in components.items()
        }
        summary: dict[str, str] = {}
        diagnostics: list[dict[str, Any]] = []
        ok = True
        for component in DEFAULT_HEALTH_COMPONENTS:
            present = normalized.get(component.value) is not None
            summary[component.value] = "ok" if present else "missing"
            if not present:
                diagnostic = {
                    "component": component.value,
                    "status": "missing",
                    "critical": component in self.critical_components,
                }
                diagnostics.append(diagnostic)
                if component in self.critical_components:
                    ok = False
        report = ComponentHealthReport(ok=ok, summary=summary, diagnostics=diagnostics)
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record("component.health_checked", report.to_dict())
        if not ok and any(item["critical"] for item in diagnostics):
            return report
        return report

    def enforce(self, components: dict[str | ComponentName, Any]) -> ComponentHealthReport:
        report = self.check(components)
        if not report.ok:
            raise ComponentHealthError(
                "Component health check failed.",
                code="component_health_failed",
                details=report.to_dict(),
            )
        return report
