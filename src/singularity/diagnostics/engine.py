from __future__ import annotations

from collections.abc import Iterable
from pathlib import Path

from singularity.diagnostics.checks import default_checks
from singularity.diagnostics.models import (
    DiagnosticCheck,
    DiagnosticContext,
    DiagnosticFinding,
    DiagnosticResult,
    DiagnosticSeverity,
    now_iso,
)
from singularity.release.paths import UserDataPaths


class DoctorEngine:
    def __init__(self, checks: Iterable[DiagnosticCheck]) -> None:
        self.checks = list(checks)

    @classmethod
    def default(cls) -> DoctorEngine:
        return cls(default_checks())

    def run(
        self,
        *,
        paths: UserDataPaths,
        project_root: Path,
        check_id: str | None = None,
        group: str | None = None,
    ) -> DiagnosticResult:
        selected = [
            check
            for check in self.checks
            if (check_id is None or check.check_id == check_id)
            and (group is None or check.group == group)
        ]
        context = DiagnosticContext(paths=paths, project_root=project_root.resolve(strict=False))
        if not selected:
            missing = check_id or group or "<none>"
            finding = DiagnosticFinding(
                check_id=check_id or "diagnostics.no_matching_checks",
                group=group or "diagnostics",
                severity=DiagnosticSeverity.ERROR,
                status="failed",
                message="No diagnostic checks matched the requested filter.",
                technical_detail=f"requested={missing}",
                suggested_fix="Run `sg config doctor` for public runtime diagnostics.",
                auto_repairable=False,
                details={"check_id": check_id, "group": group},
            )
            return DiagnosticResult(
                ok=False,
                generated_at=now_iso(),
                filters={"check_id": check_id, "group": group},
                summary={severity.value: int(severity == DiagnosticSeverity.ERROR) for severity in DiagnosticSeverity},
                findings=[finding],
            )
        findings: list[DiagnosticFinding] = []
        for check in selected:
            try:
                raw = check.run(context)
                if isinstance(raw, DiagnosticFinding):
                    findings.append(raw)
                else:
                    findings.extend(list(raw))
            except Exception as exc:
                findings.append(
                    DiagnosticFinding(
                        check_id=check.check_id,
                        group=check.group,
                        severity=DiagnosticSeverity.ERROR,
                        status="failed",
                        message=f"{check.check_id} crashed.",
                        technical_detail=f"{type(exc).__name__}: {exc}",
                        suggested_fix="Inspect the check implementation or rerun with --verbose.",
                        auto_repairable=False,
                        details={"error_type": type(exc).__name__},
                    )
                )
        summary = {severity.value: 0 for severity in DiagnosticSeverity}
        for finding in findings:
            summary[finding.severity.value] += 1
        return DiagnosticResult(
            ok=not any(finding.failed_error for finding in findings),
            generated_at=now_iso(),
            filters={"check_id": check_id, "group": group},
            summary=summary,
            findings=findings,
        )
