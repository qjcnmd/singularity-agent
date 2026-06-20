from __future__ import annotations

import json
import re
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from miniharness.observability.redaction import TraceRedactor
from miniharness.policy.config import PolicyConfig
from miniharness.policy.models import (
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
        self.path = Path(config.audit_log_path)

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
        entry = PolicyAuditEntry(
            timestamp=datetime.now(UTC).isoformat(),
            session_id=request.session_id,
            task_id=request.task_id,
            phase_id=request.phase_id,
            action_id=request.action_id,
            request_id=request.request_id,
            decision_id=decision.decision_id,
            runtime=request.runtime,
            operation=request.operation,
            capability=request.capability,
            resource_summary=redact_resource_identifier(request.resource.identifier),
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
        redacted = _TRACE_REDACTOR.redact_text(value)
        return SECRET_VALUE_RE.sub(lambda match: (match.group(1) if match.group(1) else "") + "[REDACTED]", redacted)
    return value
