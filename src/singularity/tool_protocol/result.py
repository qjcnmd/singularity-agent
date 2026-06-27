from __future__ import annotations

import json
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.context.redaction import ContextRedactor
from singularity.tool_protocol.models import (
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolProtocolResultEnvelope,
    envelope_from_tool_result,
)
from singularity.tools.models import ToolResult


class ToolProtocolResultBuilder:
    def __init__(
        self,
        artifact_root: Path | None = None,
        *,
        redactor: ContextRedactor | None = None,
        max_preview_chars: int = 2000,
    ) -> None:
        self.artifact_root = artifact_root
        self.redactor = redactor or ContextRedactor()
        self.max_preview_chars = max_preview_chars
        if artifact_root is not None:
            artifact_root.mkdir(parents=True, exist_ok=True)

    def build(
        self,
        *,
        tool_call: ToolCallEnvelope | None = None,
        envelope: ToolCallEnvelope | None = None,
        result: ToolResult,
        redact: bool = True,
        raw_result_ref: str | None = None,
        observation_id: str | None = None,
        policy_decision_id: str | None = None,
        approval_grant_id: str | None = None,
    ) -> ToolProtocolResultEnvelope:
        tool_call = tool_call or envelope
        if tool_call is None:
            raise ValueError("tool_call is required")
        raw_payload = result.model_dump(mode="json")
        raw_ref = raw_result_ref or self._persist_raw(tool_call, raw_payload)
        # The full failure remains available to trusted trace/artifact consumers, but
        # policy decisions, approval grants, matchers and backend capabilities are
        # authority-bearing internal objects.  A model observation only receives the
        # stable public error contract.
        if result.ok:
            preview_value: Any = result.content
        else:
            error = result.error
            preview_value = {
                "code": error.code if error is not None else "tool_executor_failed",
                "message": error.message if error is not None else "Tool execution failed.",
            }
        redacted_preview = self.redactor.redact_value(preview_value) if redact else preview_value
        preview = json.dumps(redacted_preview, ensure_ascii=False, default=str)
        truncated = result.truncated or len(preview) > self.max_preview_chars
        if len(preview) > self.max_preview_chars:
            preview = preview[: self.max_preview_chars]
        digest = self.redactor.hash_value(raw_payload)
        error_kind = None if result.ok else ToolCallFailureKind.tool_executor_failed
        return envelope_from_tool_result(
            tool_call=tool_call,
            result=result,
            status="ok" if result.ok else "failed",
            content_preview=preview,
            content_digest=digest,
            raw_result_ref=raw_ref,
            observation_id=observation_id,
            redacted=redact,
            truncated=truncated,
            error_kind=error_kind,
            policy_decision_id=policy_decision_id,
            approval_grant_id=approval_grant_id,
            metadata={"result_ref_stored": raw_ref is not None},
        )

    def _persist_raw(self, tool_call: ToolCallEnvelope, payload: dict[str, Any]) -> str | None:
        if self.artifact_root is None:
            return None
        result_id = f"tool_result_{uuid4().hex[:12]}"
        path = self.artifact_root / f"{result_id}.json"
        redacted_payload = self.redactor.redact_value(payload)
        path.write_text(
            json.dumps(redacted_payload, ensure_ascii=False, indent=2, default=str),
            encoding="utf-8",
        )
        return str(path)
