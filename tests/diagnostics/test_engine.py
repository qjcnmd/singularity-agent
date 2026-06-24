from __future__ import annotations

import json

from singularity.diagnostics import (
    DiagnosticCheck,
    DiagnosticContext,
    DiagnosticFinding,
    DiagnosticSeverity,
    DoctorEngine,
)
from singularity.release.paths import resolve_user_data_paths


def test_doctor_engine_filters_and_converts_crashing_check_to_finding(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")

    def ok_check(context: DiagnosticContext) -> DiagnosticFinding:
        return DiagnosticFinding(
            check_id="environment.test_ok",
            group="environment",
            severity=DiagnosticSeverity.INFO,
            status="passed",
            message="ok",
            technical_detail="ran",
            suggested_fix="none",
            auto_repairable=False,
        )

    def boom(context: DiagnosticContext):
        raise RuntimeError("boom")

    engine = DoctorEngine(
        checks=[
            DiagnosticCheck(
                check_id="environment.test_ok",
                group="environment",
                severity=DiagnosticSeverity.INFO,
                run=ok_check,
            ),
            DiagnosticCheck(
                check_id="config.crash",
                group="config",
                severity=DiagnosticSeverity.ERROR,
                run=boom,
            ),
        ]
    )

    filtered = engine.run(paths=paths, project_root=tmp_path, group="environment")
    crashed = engine.run(paths=paths, project_root=tmp_path)

    assert [finding.check_id for finding in filtered.findings] == ["environment.test_ok"]
    assert crashed.ok is False
    crash = next(item for item in crashed.findings if item.check_id == "config.crash")
    assert crash.status == "failed"
    assert crash.severity == DiagnosticSeverity.ERROR
    assert "RuntimeError" in crash.technical_detail


def test_diagnostic_result_json_shape_is_stable(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="environment.python")

    payload = json.loads(result.to_json())

    assert payload["schema_version"] == "diagnostic-result/v1"
    assert payload["filters"] == {"check_id": "environment.python", "group": None}
    assert {"error", "warning", "info", "suggestion"} <= set(payload["summary"])
    assert payload["findings"][0]["check_id"] == "environment.python"
    assert payload["findings"][0]["group"] == "environment"
    assert "technical_detail" in payload["findings"][0]
    assert payload["checks"][0]["name"] == "python_version"
    assert payload["checks"][0]["check_id"] == "environment.python"
    assert payload["checks"][0]["status"] == "ok"


def test_doctor_engine_reports_unknown_check_as_error(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="missing.check")

    assert result.ok is False
    assert result.findings[0].check_id == "missing.check"
    assert result.findings[0].severity == DiagnosticSeverity.ERROR
