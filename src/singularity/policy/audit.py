from __future__ import annotations

import json
import re
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.observability.redaction import TraceRedactor
from singularity.policy.config import PolicyConfig
from singularity.policy.models import (
    ApprovalGrant,
    PolicyAuditEntry,
    PolicyDecision,
    PolicyRequest,
    stable_hash,
)


SECRET_KEY_RE = re.compile(r"(authorization|token|api[_-]?key|secret|password)", re.IGNORECASE)
SECRET_VALUE_RE = re.compile(
    r"(Bearer\s+)[A-Za-z0-9._\-]+|sk-[A-Za-z0-9._\-]+|secret-token",
    re.IGNORECASE,
)
SENSITIVE_PATH_RE = re.compile(
    r"(^\.env(?:\..*)?$|(^|[\\/])\.ssh([\\/]|$)|(^|[\\/])\.gnupg([\\/]|$)|"
    r"(^|[\\/])\.aws([\\/]|$)|(^|[\\/])\.azure([\\/]|$)|id_rsa|id_dsa|id_ecdsa|id_ed25519|"
    r"credentials?|credential|token|secret|api[_-]?key|password|\.pem$|\.pfx$|\.p12$|\.key$)",
    re.IGNORECASE,
)
_TRACE_REDACTOR = TraceRedactor()


class PolicyAuditWriter:
    def __init__(self, config: PolicyConfig) -> None:
        self.path = _audit_log_path(config)

    def append(
        self,
        *,
        request: PolicyRequest,
        decision: PolicyDecision,
        grant: ApprovalGrant | None = None,
        user_decision: str | None = None,
        execution_result_ref: str | None = None,
    ) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        request_payload = redact(request.to_dict())
        resource_summaries = _resource_summaries(request)
        entry = PolicyAuditEntry(
            timestamp=datetime.now(UTC).isoformat(),
            session_id=request.session_id,
            task_id=request.task_id,
            phase_id=request.phase_id,
            action_id=request.action_id,
            request_id=request.request_id,
            decision_id=decision.decision_id,
            component=request.component,
            operation=request.operation,
            capability=request.capability,
            resource_summary=", ".join(resource_summaries)
            if resource_summaries
            else redact_resource_identifier(request.resource.identifier),
            normalized_input_hash=stable_hash(request_payload),
            risk_level=decision.risk_level,
            risk_tags=decision.risk_tags,
            outcome=decision.outcome,
            rule_ids=decision.rule_ids,
            reason=redact(decision.reason),
            approval_required=decision.required_approval is not None,
            approval_grant_id=grant.grant_id if grant else decision.approval_grant_id,
            approved_by_user=grant is not None,
            user_decision=user_decision,
            constraints=decision.constraints.to_dict(),
            execution_result_ref=execution_result_ref,
        )
        payload = redact(entry.to_dict())
        payload["request_summary"] = redact(
            {
                "resource": redact_resource_identifier(request.resource.identifier),
                "resources": resource_summaries,
                "reason": request.reason,
                "metadata": request.metadata,
            }
        )
        with self.path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(payload, ensure_ascii=False, default=str) + "\n")


def redact_resource_identifier(value: str) -> str:
    normalized = value.replace("\\", "/")
    if SENSITIVE_PATH_RE.search(normalized):
        return "<redacted>"
    return redact(value)


def redact(value: Any) -> Any:
    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if SECRET_KEY_RE.search(str(key)):
                redacted[key] = "[REDACTED]"
            else:
                redacted[key] = redact(item)
        return redacted
    if isinstance(value, list):
        return [redact(item) for item in value]
    if isinstance(value, str):
        redacted_text = _TRACE_REDACTOR.redact_text(value)
        return SECRET_VALUE_RE.sub(lambda match: (match.group(1) if match.group(1) else "") + "[REDACTED]", redacted_text)
    return value


def _audit_log_path(config: PolicyConfig) -> Path:
    if config.audit_log_path is None:
        # P0-1: Default audit log must live outside the model-writable
        # workspace so the model cannot tamper with audit entries.
        from singularity.policy.config import _default_policy_home
        return _default_policy_home() / ".singularity" / "policy" / "audit.jsonl"
    return Path(config.audit_log_path)


def _resource_summaries(request: PolicyRequest) -> list[str]:
    resources = request.metadata.get("resources")
    if not isinstance(resources, list):
        return []
    summaries: list[str] = []
    for item in resources:
        if not isinstance(item, dict):
            continue
        identifier = item.get("normalized_identifier") or item.get("identifier")
        if identifier is not None:
            summaries.append(redact_resource_identifier(str(identifier)))
    return summaries
