from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from miniharness.kernel.exceptions import RuntimeHealthError
from miniharness.kernel.models import RuntimeComponentName


DEFAULT_HEALTH_COMPONENTS = [
    RuntimeComponentName.CONFIGURATION,
    RuntimeComponentName.OBSERVABILITY,
    RuntimeComponentName.INTERACTION,
    RuntimeComponentName.WORKSPACE_STATE,
    RuntimeComponentName.PROJECT_INDEX,
    RuntimeComponentName.POLICY,
    RuntimeComponentName.SANDBOX,
    RuntimeComponentName.COMMAND,
    RuntimeComponentName.MUTATION,
    RuntimeComponentName.EDIT,
    RuntimeComponentName.TOOLS,
    RuntimeComponentName.VERIFICATION,
    RuntimeComponentName.REVIEW,
    RuntimeComponentName.INSTRUCTIONS,
    RuntimeComponentName.MODEL,
    RuntimeComponentName.PLANNER,
]


@dataclass(frozen=True)
class RuntimeHealthReport:
    ok: bool
    summary: dict[str, str]
    diagnostics: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "summary": self.summary,
            "diagnostics": self.diagnostics,
        }


class RuntimeHealthChecker:
    def __init__(
        self,
        *,
        trace: Any | None = None,
        critical_components: set[RuntimeComponentName] | None = None,
    ) -> None:
        self.trace = trace
        self.critical_components = (
            critical_components
            if critical_components is not None
            else set(DEFAULT_HEALTH_COMPONENTS)
        )

    def check(self, components: dict[str | RuntimeComponentName, Any]) -> RuntimeHealthReport:
        normalized = {
            key.value if isinstance(key, RuntimeComponentName) else str(key): value
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
        report = RuntimeHealthReport(ok=ok, summary=summary, diagnostics=diagnostics)
        if self.trace is not None and hasattr(self.trace, "record"):
            self.trace.record("runtime.health_checked", report.to_dict())
        if not ok and any(item["critical"] for item in diagnostics):
            return report
        return report

    def enforce(self, components: dict[str | RuntimeComponentName, Any]) -> RuntimeHealthReport:
        report = self.check(components)
        if not report.ok:
            raise RuntimeHealthError(
                "Runtime health check failed.",
                code="runtime_health_failed",
                details=report.to_dict(),
            )
        return report
