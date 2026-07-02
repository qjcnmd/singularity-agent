from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Any


class SessionStatus(StrEnum):
    ACTIVE = "active"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"
    INTERRUPTED = "interrupted"
    NEEDS_REVIEW = "needs_review"


class SessionState(StrEnum):
    ACTIVE = "active"
    RECOVERABLE = "recoverable"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"
    CLOSED = "closed"


class SessionRunMode(StrEnum):
    NEW = "new"
    CONTINUE = "continue"
    RESUME = "resume"


class SessionCheckpointKind(StrEnum):
    WORKSPACE = "workspace"
    CONTEXT = "context"
    TOOL_PROTOCOL = "tool_protocol"
    VERIFICATION = "verification"
    RECOVERY_GATE = "recovery_gate"


class RecoveryGateStatus(StrEnum):
    READY_TO_CONTINUE = "ready_to_continue"
    READY_TO_RESUME = "ready_to_resume"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"


@dataclass(frozen=True)
class SessionSummary:
    session_id: str
    project_root: str
    user_goal: str
    task_id: str
    status: SessionStatus
    state: SessionState
    created_at: str
    updated_at: str
    last_run_id: str | None = None
    last_task_status: str | None = None
    continue_command: str = ""
    resume_command: str = ""
    show_command: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "project_root": self.project_root,
            "user_goal": self.user_goal,
            "task_id": self.task_id,
            "status": self.status.value,
            "state": self.state.value,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "last_run_id": self.last_run_id,
            "last_task_status": self.last_task_status,
            "continue_command": self.continue_command,
            "resume_command": self.resume_command,
            "show_command": self.show_command,
        }


@dataclass(frozen=True)
class SessionRun:
    run_id: str
    session_id: str
    task_id: str
    mode: SessionRunMode
    user_goal: str
    trace_run_dir: str
    status: SessionStatus
    started_at: str
    ended_at: str | None = None
    final_report_ref: str | None = None
    summary: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "mode": self.mode.value,
            "user_goal": self.user_goal,
            "trace_run_dir": self.trace_run_dir,
            "status": self.status.value,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "final_report_ref": self.final_report_ref,
            "summary": self.summary,
        }


@dataclass(frozen=True)
class SessionCheckpoint:
    checkpoint_id: str
    session_id: str
    run_id: str
    task_id: str
    kind: SessionCheckpointKind
    summary: str
    payload: dict[str, Any]
    created_at: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "checkpoint_id": self.checkpoint_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "task_id": self.task_id,
            "kind": self.kind.value,
            "summary": self.summary,
            "payload": self.payload,
            "created_at": self.created_at,
        }


@dataclass(frozen=True)
class SessionTimelineEvent:
    event_id: str
    session_id: str
    run_id: str | None
    task_id: str | None
    event_type: str
    summary: str
    payload: dict[str, Any]
    created_at: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "session_id": self.session_id,
            "run_id": self.run_id,
            "task_id": self.task_id,
            "event_type": self.event_type,
            "summary": self.summary,
            "payload": self.payload,
            "created_at": self.created_at,
        }


@dataclass(frozen=True)
class SessionDetail:
    session: SessionSummary
    runs: list[SessionRun]
    checkpoints: list[SessionCheckpoint]
    timeline: list[SessionTimelineEvent]

    def to_dict(self) -> dict[str, Any]:
        return {
            "session": self.session.to_dict(),
            "runs": [run.to_dict() for run in self.runs],
            "checkpoints": [checkpoint.to_dict() for checkpoint in self.checkpoints],
            "timeline": [event.to_dict() for event in self.timeline],
        }


@dataclass(frozen=True)
class SessionResumeContext:
    session_id: str
    user_goal: str = ""
    current_instruction: str = ""
    dialogue_summary: list[dict[str, str]] = field(default_factory=list)
    planner: dict[str, Any] = field(default_factory=dict)
    workspace: dict[str, Any] = field(default_factory=dict)
    verification: dict[str, Any] = field(default_factory=dict)
    tool_protocol: dict[str, Any] = field(default_factory=dict)
    failures: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_sources(
        cls,
        *,
        session_id: str,
        user_goal: str,
        current_instruction: str | None = None,
        dialogue: list[dict[str, Any]] | None = None,
        planner: dict[str, Any] | None = None,
        workspace: dict[str, Any] | None = None,
        verification: dict[str, Any] | None = None,
        tool_protocol: dict[str, Any] | None = None,
        failures: dict[str, Any] | None = None,
    ) -> SessionResumeContext:
        safe_dialogue: list[dict[str, str]] = []
        for message in dialogue or []:
            role = str(message.get("role") or "")
            if role not in {"user", "assistant"}:
                continue
            content = str(message.get("content") or "")
            if content:
                safe_dialogue.append(
                    {"role": role, "content": _sanitize_resume_text(content)[:1000]}
                )
        return cls(
            session_id=session_id,
            user_goal=_sanitize_resume_text(str(user_goal)),
            current_instruction=_sanitize_resume_text(str(current_instruction or "")),
            dialogue_summary=safe_dialogue[-12:],
            planner=_safe_payload(planner or {}),
            workspace=_safe_payload(workspace or {}),
            verification=_safe_payload(verification or {}),
            tool_protocol=_safe_payload(tool_protocol or {}),
            failures=_safe_payload(failures or {}),
        )

    def to_model_context(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "user_goal": self.user_goal,
            "current_instruction": self.current_instruction,
            "dialogue_summary": self.dialogue_summary,
            "planner_summary": self.planner,
            "workspace_summary": self.workspace,
            "verification_summary": self.verification,
            "tool_protocol_summary": self.tool_protocol,
            "failure_summary": self.failures,
        }

    def to_dict(self) -> dict[str, Any]:
        return self.to_model_context()


@dataclass(frozen=True)
class RecoveryGateDecision:
    session_id: str
    mode: str
    status: RecoveryGateStatus
    can_call_model: bool
    blockers: list[str]
    warnings: list[str]
    next_action: str
    resume_context: SessionResumeContext

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "mode": self.mode,
            "status": self.status.value,
            "can_call_model": self.can_call_model,
            "blockers": self.blockers,
            "warnings": self.warnings,
            "next_action": self.next_action,
            "resume_context": self.resume_context.to_dict(),
        }


@dataclass(frozen=True)
class SessionLaunch:
    session_id: str
    task_id: str
    run_id: str
    mode: SessionRunMode
    user_goal: str
    previous_run_id: str | None = None
    previous_status: str | None = None
    previous_trace_run_dir: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "task_id": self.task_id,
            "run_id": self.run_id,
            "mode": self.mode.value,
            "user_goal": self.user_goal,
            "previous_run_id": self.previous_run_id,
            "previous_status": self.previous_status,
            "previous_trace_run_dir": self.previous_trace_run_dir,
        }


def now_iso() -> str:
    return datetime.now(UTC).isoformat()


def normalize_path(path: Path | str) -> str:
    return str(Path(path).expanduser().resolve(strict=False))


def session_state_for_status(status: SessionStatus) -> SessionState:
    if status == SessionStatus.INTERRUPTED:
        return SessionState.RECOVERABLE
    if status == SessionStatus.NEEDS_REVIEW:
        return SessionState.NEEDS_REVIEW
    if status in {SessionStatus.BLOCKED, SessionStatus.FAILED, SessionStatus.CANCELLED}:
        return SessionState.BLOCKED
    if status == SessionStatus.COMPLETED:
        return SessionState.CLOSED
    return SessionState.ACTIVE


def _safe_payload(payload: dict[str, Any]) -> dict[str, Any]:
    denied_exact = {
        "args",
        "arguments",
        "env",
        "environment",
        "stdout",
        "stderr",
        "output",
        "output_excerpt",
        "raw_args",
        "raw_arguments",
        "raw_result",
        "result",
        "token",
        "api_key",
        "secret",
    }
    env_status_keys = {"env_status", "provider_env_status", "provider_status", "environment_status"}
    safe: dict[str, Any] = {}
    for key, value in payload.items():
        key_text = str(key)
        lowered = key_text.lower()
        if lowered in env_status_keys:
            safe[key_text] = _safe_env_status(value)
            continue
        if lowered in denied_exact or lowered.startswith("raw_") or _sensitive_field_name(lowered):
            continue
        if isinstance(value, dict):
            safe[key_text] = _safe_payload(value)
        elif isinstance(value, list):
            safe[key_text] = [
                _safe_payload(item) if isinstance(item, dict) else item
                for item in value[:50]
            ]
            safe[key_text] = _safe_value(safe[key_text])
        else:
            safe[key_text] = _safe_value(value)
    return safe


SECRET_VALUE_RE = re.compile(
    r"\b("
    r"sk-[A-Za-z0-9._\-]+"
    r"|gh[pousr]_[A-Za-z0-9_]+"
    r"|npm_[A-Za-z0-9_]+"
    r"|AKIA[0-9A-Z]{16}"
    r"|eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+"
    r"|xox[baprs]-[A-Za-z0-9-]+"
    r"|sk_live_[A-Za-z0-9]+"
    r"|AIza[0-9A-Za-z_-]{35}"
    r")\b"
)
SECRET_ASSIGNMENT_RE = re.compile(
    r"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD))\s*=\s*([^\s;,\]\}]+)"
)
ENV_ASSIGNMENT_RE = re.compile(
    r"(?im)(^|[;\n]\s*)(?:(?:export|set)\s+|\$env:)?([A-Z_][A-Z0-9_]{1,})\s*=\s*([^\r\n;]+)"
)
AUTH_HEADER_RE = re.compile(r"(?i)\b(authorization\s*:\s*bearer\s+)([A-Za-z0-9._~+/=-]+)")


def _safe_value(value: Any) -> Any:
    if isinstance(value, str):
        return _sanitize_resume_text(value)
    if isinstance(value, list):
        return [_safe_value(item) for item in value[:50]]
    if isinstance(value, dict):
        return _safe_payload(value)
    return value


def _sensitive_field_name(key: str) -> bool:
    if key.endswith("_tokens") or key in {"input_tokens", "output_tokens", "total_tokens"}:
        return False
    markers = (
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "password",
        "private_key",
        "refresh_token",
        "access_token",
        "client_secret",
    )
    return any(marker in key for marker in markers)


def _safe_env_status(value: Any) -> dict[str, str]:
    if isinstance(value, str):
        parsed = _parse_env_status_text(value)
        if parsed:
            return parsed
        return {"summary": _sanitize_resume_text(value)}
    if isinstance(value, dict):
        status: dict[str, str] = {}
        for key, item in value.items():
            key_text = str(key)
            if not _looks_env_name(key_text):
                continue
            status[key_text] = _normalize_status_value(item, secret=_env_name_is_secret(key_text))
        return status
    return {}


def _parse_env_status_text(text: str) -> dict[str, str]:
    status: dict[str, str] = {}
    for match in ENV_ASSIGNMENT_RE.finditer(text):
        key = match.group(2)
        raw_value = match.group(3).strip().strip("'\"")
        status[key] = _normalize_status_value(raw_value, secret=_env_name_is_secret(key))
    return status


def _sanitize_resume_text(text: str) -> str:
    value = SECRET_VALUE_RE.sub("<redacted>", text)
    value = AUTH_HEADER_RE.sub(lambda match: f"{match.group(1)}<redacted>", value)
    value = SECRET_ASSIGNMENT_RE.sub(lambda match: f"{match.group(1)} <redacted>", value)

    def replace_env(match: re.Match[str]) -> str:
        prefix = match.group(1)
        key = match.group(2)
        raw = match.group(3).strip().strip("'\"")
        status = _normalize_status_value(raw, secret=_env_name_is_secret(key))
        return f"{prefix}{key} {status}"

    return ENV_ASSIGNMENT_RE.sub(replace_env, value)


def _normalize_status_value(value: Any, *, secret: bool) -> str:
    text = str(value or "").strip().strip("'\"").lower()
    if text in {"present", "set", "configured", "available", "true", "yes"}:
        return "present_redacted" if secret else "present"
    if text in {"present(redacted)", "present_redacted", "<redacted>", "[redacted]", "redacted"}:
        return "present_redacted" if secret else "redacted"
    if text in {"missing", "unset", "not_set", "absent", "false", "no", ""}:
        return "missing"
    if secret or SECRET_VALUE_RE.search(str(value)):
        return "present_redacted"
    return "present"


def _looks_env_name(value: str) -> bool:
    return bool(re.fullmatch(r"[A-Z_][A-Z0-9_]{1,}", value))


def _env_name_is_secret(value: str) -> bool:
    return bool(re.search(r"(API[_-]?KEY|TOKEN|SECRET|PASSWORD|PRIVATE[_-]?KEY)", value, re.I))
