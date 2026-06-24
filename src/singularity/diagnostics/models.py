from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Iterable

from singularity.release.paths import UserDataPaths


DIAGNOSTIC_RESULT_SCHEMA = "diagnostic-result/v1"
REPAIR_PLAN_SCHEMA = "repair-plan/v1"


class DiagnosticSeverity(str, Enum):
    ERROR = "error"
    WARNING = "warning"
    INFO = "info"
    SUGGESTION = "suggestion"


@dataclass(frozen=True)
class DiagnosticContext:
    paths: UserDataPaths
    project_root: Path


@dataclass(frozen=True)
class DiagnosticFinding:
    check_id: str
    group: str
    severity: DiagnosticSeverity | str
    status: str
    message: str
    technical_detail: str
    suggested_fix: str
    auto_repairable: bool
    details: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "severity", _severity(self.severity))

    @property
    def failed_error(self) -> bool:
        return self.status == "failed" and self.severity == DiagnosticSeverity.ERROR

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


CheckRun = Callable[[DiagnosticContext], "DiagnosticFinding | Iterable[DiagnosticFinding]"]


@dataclass(frozen=True)
class DiagnosticCheck:
    check_id: str
    group: str
    severity: DiagnosticSeverity | str
    run: CheckRun

    def __post_init__(self) -> None:
        object.__setattr__(self, "severity", _severity(self.severity))


@dataclass(frozen=True)
class DiagnosticResult:
    ok: bool
    generated_at: str
    filters: dict[str, str | None]
    summary: dict[str, int]
    findings: list[DiagnosticFinding]
    schema_version: str = DIAGNOSTIC_RESULT_SCHEMA

    def to_dict(self) -> dict[str, Any]:
        findings = [finding.to_dict() for finding in self.findings]
        return {
            "schema_version": self.schema_version,
            "ok": self.ok,
            "generated_at": self.generated_at,
            "filters": dict(self.filters),
            "summary": dict(self.summary),
            "findings": findings,
            "checks": [_legacy_check(item) for item in findings],
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n"


@dataclass(frozen=True)
class RepairAction:
    action_id: str
    check_id: str
    description: str
    risk: str
    kind: str
    target: str
    params: dict[str, Any] = field(default_factory=dict)
    status: str = "planned"
    message: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass(frozen=True)
class RepairPlan:
    actions: list[RepairAction]
    blocked_actions: list[dict[str, Any]] = field(default_factory=list)
    applied: bool = False
    audit_log_path: str | None = None
    generated_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    schema_version: str = REPAIR_PLAN_SCHEMA

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "generated_at": self.generated_at,
            "applied": self.applied,
            "audit_log_path": self.audit_log_path,
            "actions": [action.to_dict() for action in self.actions],
            "blocked_actions": list(self.blocked_actions),
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def now_iso() -> str:
    return datetime.now(UTC).isoformat()


def _severity(value: DiagnosticSeverity | str) -> DiagnosticSeverity:
    if isinstance(value, DiagnosticSeverity):
        return value
    return DiagnosticSeverity(str(value))


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return str(value)
    if is_dataclass(value):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, dict):
        return {str(key): _to_plain(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_to_plain(item) for item in value]
    return value


def _legacy_check(finding: dict[str, Any]) -> dict[str, Any]:
    status = "ok" if finding["status"] == "passed" else "error" if finding["severity"] == "error" else "warning"
    suggested_fix = finding["suggested_fix"]
    legacy_name = _legacy_check_name(finding["check_id"])
    return {
        "name": legacy_name,
        "check_id": finding["check_id"],
        "group": finding["group"],
        "severity": finding["severity"],
        "status": status,
        "ok": status == "ok",
        "message": finding["message"],
        "suggestion": None if suggested_fix == "No action needed." else suggested_fix,
        "technical_detail": finding["technical_detail"],
        "suggested_fix": suggested_fix,
        "auto_repairable": finding["auto_repairable"],
        "details": finding["details"],
    }


def _legacy_check_name(check_id: str) -> str:
    return {
        "environment.python": "python_version",
        "environment.package": "cli_installation",
        "environment.optional_dependencies": "optional_dependencies",
        "config.file": "config_schema",
        "config.provider": "component_configuration",
        "filesystem.user_data_dirs": "user_data_directories",
        "schema.migrations": "migrations",
    }.get(check_id, check_id)
