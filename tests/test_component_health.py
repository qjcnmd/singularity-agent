from __future__ import annotations

from singularity.kernel.health import DEFAULT_HEALTH_COMPONENTS, ComponentHealthChecker
from singularity.kernel.models import ComponentName


def test_component_health_checker_reports_missing_critical_components() -> None:
    checker = ComponentHealthChecker(critical_components={ComponentName.CONFIGURATION})

    report = checker.check({"trace": object()})

    assert report.ok is False
    assert report.summary["config"] == "missing"
    assert report.summary["trace"] == "ok"
    assert report.diagnostics[0]["component"] == "config"


def test_component_health_checker_records_trace_event() -> None:
    class Trace:
        def __init__(self) -> None:
            self.events: list[tuple[str, dict]] = []

        def record(self, event: str, data: dict) -> None:
            self.events.append((event, data))

    trace = Trace()
    checker = ComponentHealthChecker(trace=trace)

    components = {component.value: object() for component in DEFAULT_HEALTH_COMPONENTS}
    components["trace"] = trace
    report = checker.check(components)

    assert report.ok is True
    assert trace.events[-1][0] == "component.health_checked"
    assert trace.events[-1][1]["ok"] is True


def test_component_health_checker_fails_closed_for_missing_component_components() -> None:
    checker = ComponentHealthChecker()

    report = checker.check({"config": object(), "trace": object()})

    assert report.ok is False
    assert report.summary["workspace"] == "missing"
    assert any(item["component"] == "workspace" for item in report.diagnostics)
