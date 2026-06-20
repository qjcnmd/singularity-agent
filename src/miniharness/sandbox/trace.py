from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from miniharness.sandbox.models import PreparedSandbox, SandboxCapabilities, SandboxRequest, SandboxResult


class SandboxTraceWriter:
    def __init__(self, path: Path) -> None:
        self.path = path

    @classmethod
    def create(cls, workspace_root: Path) -> "SandboxTraceWriter":
        path = workspace_root / ".miniharness" / "sandbox" / "trace.jsonl"
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
            "cwd": str(request.cwd) if request else None,
            "workspace_root": str(request.workspace_root) if request else None,
            "sandbox_root": str(prepared.sandbox_root) if prepared else None,
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
            "policy_decision_id": request.policy_decision_id if request else None,
        }
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(_redact(entry), ensure_ascii=False, default=str) + "\n")


def _command_summary(command: list[str] | str | None) -> str | list[str] | None:
    if command is None:
        return None
    if isinstance(command, list):
        return [_redact_text(str(part)) for part in command[:20]]
    return _redact_text(command)


def _redact(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: _redact_text(str(item)) if _secret_key(str(key)) else _redact(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_redact(item) for item in value]
    if isinstance(value, str):
        return _redact_text(value)
    return value


def _secret_key(key: str) -> bool:
    upper = key.upper()
    return any(token in upper for token in ("TOKEN", "KEY", "SECRET", "PASSWORD", "COOKIE", "AUTHORIZATION"))


def _redact_text(text: str) -> str:
    redacted = text
    for marker in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GITHUB_TOKEN", "NPM_TOKEN", "PASSWORD", "SECRET", "TOKEN", "COOKIE", "AUTHORIZATION"):
        if marker.lower() in redacted.lower():
            return "[REDACTED]"
    return redacted
