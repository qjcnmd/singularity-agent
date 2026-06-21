from __future__ import annotations

from typing import Any

from singularity.edit.models import EditIntent, EditOperation, EditScope


def intent_from_payload(payload: dict[str, Any]) -> EditIntent:
    scope_payload = payload.get("scope") or {}
    return EditIntent(
        summary=str(payload.get("summary") or payload.get("intent") or "edit workspace"),
        operations=[
            item if isinstance(item, EditOperation) else EditOperation(**dict(item))
            for item in payload.get("operations") or []
        ],
        scope=scope_payload if isinstance(scope_payload, EditScope) else EditScope(**scope_payload),
        strategy=payload.get("strategy"),
        actor=str(payload.get("actor") or "agent"),
        metadata=dict(payload.get("metadata") or {}),
    )
