from __future__ import annotations

import hashlib
import json

from singularity.tool_protocol.models import (
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolProtocolResultEnvelope,
)


class ToolProtocolSyntheticResultFactory:
    def create(
        self,
        envelope: ToolCallEnvelope,
        *,
        error_kind: ToolCallFailureKind,
        message: str,
        error_code: str | None,
    ) -> ToolProtocolResultEnvelope:
        digest = hashlib.sha256(
            json.dumps(
                {
                    "tool_call_id": envelope.tool_call_id,
                    "tool_name": envelope.tool_name,
                    "error_kind": error_kind.value,
                    "error_code": error_code,
                    "message": message,
                },
                ensure_ascii=False,
                sort_keys=True,
                default=str,
            ).encode("utf-8")
        ).hexdigest()
        return ToolProtocolResultEnvelope(
            tool_call_id=envelope.tool_call_id,
            tool_name=envelope.tool_name,
            ok=False,
            status="rejected",
            error_code=error_code,
            error_kind=error_kind,
            content_preview=message,
            content_digest=digest,
            raw_result_ref=None,
            artifact_refs=[],
            truncated=False,
            redacted=True,
            metadata={"validation_errors": list(envelope.validation_errors), "synthetic": True},
        )
