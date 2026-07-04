from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.observability.redaction import TraceRedactor
from singularity.sandbox.models import PreparedSandbox, SandboxCapabilities, SandboxRequest, SandboxResult

_REDACTOR = TraceRedactor()


class SandboxJsonlTraceRecorder:
    def __init__(self, path: Path) -> None:
        self.path = path

    @classmethod
    def create(cls, workspace_root: Path) -> SandboxJsonlTraceRecorder:
        path = workspace_root / ".singularity" / "sandbox" / "trace.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        return cls(path)

    def append(
        self,
        *,
        prepared: PreparedSandbox | None,
        result: SandboxResult,
        capabilities: SandboxCapabilities | None = None,
        request: SandboxRequest | None = None,
    ) -> None:
        request = prepared.request if prepared is not None else request
        profile = request.profile if request is not None else None
        entry: dict[str, Any] = {
            "timestamp": datetime.now(UTC).isoformat(),
            "sandbox_id": result.sandbox_id,
            "session_id": request.session_id if request else None,
            "task_id": request.task_id if request else None,
            "action_id": request.action_id if request else None,
            "backend_name": result.backend_name,
            "profile": profile.name.value if profile else None,
            "capabilities": capabilities.to_dict() if capabilities else None,
            "command_summary": _command_summary(request.command if request else None),
            "cwd_handle": _relative_handle(request.cwd, request.workspace_root) if request else None,
            "workspace_handle": ".",
            "sandbox_handle": _relative_handle(prepared.sandbox_root, request.workspace_root)
            if prepared and request
            else None,
            "filesystem_mode": profile.filesystem.mode.value if profile else None,
            "network_mode": profile.network.mode.value if profile else None,
            "env_redaction_enabled": True,
            "timeout_seconds": profile.resources.timeout_seconds if profile else None,
            "max_output_chars": profile.resources.max_output_chars if profile else None,
            "status": result.status.value,
            "exit_code": result.exit_code,
            "duration_ms": result.duration_ms,
            "artifact_count": len(result.artifacts),
            "changed_files_count": result.filesystem_changes.total_changed_files,
            "violations": [violation.to_dict() for violation in result.violations],
            "cleanup_status": result.cleanup_status,
            "timing": dict(result.metadata.get("timing") or {}),
            "policy_decision_id": request.policy_decision_id if request else None,
        }
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(_redact(entry), ensure_ascii=False, default=str) + "\n")


def _command_summary(command: list[str] | str | None) -> str | list[str] | None:
    if command is None:
        return None
    if isinstance(command, list):
        return _redact_command_parts([str(part) for part in command[:20]])
    return _redact_text(command)


def _redact_command_parts(parts: list[str]) -> list[str]:
    return _REDACTOR.provider.redact_command_parts(parts)


def _redact(value: Any) -> Any:
    return _REDACTOR.redact_value(value)


def _redact_text(text: str) -> str:
    return _REDACTOR.redact_text(text)


def _relative_handle(path: Path, root: Path) -> str:
    try:
        return path.resolve(strict=False).relative_to(root.resolve(strict=False)).as_posix() or "."
    except ValueError:
        return path.name
