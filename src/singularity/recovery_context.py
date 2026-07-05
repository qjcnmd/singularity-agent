from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any


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
    env_status_keys = {
        "env_status",
        "provider_env_status",
        "provider_status",
        "environment_status",
    }
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
            status[key_text] = _normalize_status_value(
                item,
                secret=_env_name_is_secret(key_text),
            )
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
