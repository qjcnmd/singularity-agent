from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, is_dataclass
from enum import Enum
from typing import Any

from miniharness.review.models import ReviewEvidence, ReviewFreshness, ReviewTrustLevel


SENSITIVE_KEYS = {
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "password",
    "secret",
    "token",
}


def collect_review_evidence(**snapshots: Any) -> list[ReviewEvidence]:
    evidence: list[ReviewEvidence] = []
    for source, value in snapshots.items():
        if value is None:
            continue
        payload = to_bounded_plain(value)
        if not isinstance(payload, dict):
            payload = {"value": payload}
        evidence.append(
            ReviewEvidence(
                source=source,
                source_id=_source_id(payload),
                summary=summarize_payload(source, payload),
                payload=payload,
                payload_hash=stable_payload_hash(payload),
                artifact_ref=_artifact_ref(source, payload),
                freshness=_freshness(payload),
                trust_level=_trust_level(source),
            )
        )
    return evidence


def stable_payload_hash(payload: Any) -> str:
    text = json.dumps(to_bounded_plain(payload), ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def to_bounded_plain(value: Any, *, max_chars: int = 1800, _depth: int = 0) -> Any:
    if _depth > 6:
        return "[truncated-depth]"
    if isinstance(value, Enum):
        return value.value
    if hasattr(value, "model_dump"):
        try:
            return to_bounded_plain(value.model_dump(mode="json"), max_chars=max_chars, _depth=_depth + 1)
        except TypeError:
            return to_bounded_plain(value.model_dump(), max_chars=max_chars, _depth=_depth + 1)
    if hasattr(value, "to_dict"):
        return to_bounded_plain(value.to_dict(), max_chars=max_chars, _depth=_depth + 1)
    if is_dataclass(value):
        return to_bounded_plain(asdict(value), max_chars=max_chars, _depth=_depth + 1)
    if isinstance(value, dict):
        bounded: dict[str, Any] = {}
        for key, item in value.items():
            text_key = str(key)
            if _sensitive_key(text_key):
                bounded[text_key] = "[redacted]"
            else:
                bounded[text_key] = to_bounded_plain(item, max_chars=max_chars, _depth=_depth + 1)
        return bounded
    if isinstance(value, (list, tuple, set)):
        items = list(value)
        return [to_bounded_plain(item, max_chars=max_chars, _depth=_depth + 1) for item in items[:50]]
    if isinstance(value, str):
        if len(value) <= max_chars:
            return value
        marker = "\n...[truncated]...\n"
        head = max(0, (max_chars - len(marker)) // 2)
        tail = max(0, max_chars - len(marker) - head)
        return f"{value[:head]}{marker}{value[-tail:]}"
    if isinstance(value, (int, float, bool)) or value is None:
        return value
    return str(value)[:max_chars]


def summarize_payload(source: str, payload: dict[str, Any]) -> str:
    if source in {"validation", "patch_validation"}:
        issues = payload.get("issues") or []
        return f"Patch validation ok={payload.get('ok')} requires_review={payload.get('requires_review')} issues={len(issues)}."
    if source in {"verification", "verification_result"}:
        assessment = payload.get("completion_assessment") or {}
        return f"Verification status={assessment.get('status') or payload.get('status') or 'unknown'}."
    if source == "policy_observation":
        return f"Policy outcome={payload.get('outcome') or 'unknown'}."
    if source in {"code_impact", "project_index", "test_impact"}:
        risk = payload.get("risk_level") or payload.get("freshness") or "unknown"
        return f"{source} evidence risk_or_freshness={risk}."
    if source == "edit_result":
        return f"Edit result status={payload.get('status')} ok={payload.get('ok')}."
    return f"{source} evidence captured."


def _source_id(payload: dict[str, Any]) -> str | None:
    keys = (
        "id",
        "edit_result_id",
        "review_id",
        "verification_plan_id",
        "changeset_id",
        "transaction_id",
        "decision_id",
        "index_id",
        "patch_candidate_id",
    )
    for key in keys:
        value = payload.get(key)
        if value:
            return str(value)
    return None


def _artifact_ref(source: str, payload: dict[str, Any]) -> str | None:
    for key in ("artifact_ref", "artifact_path", "raw_response_ref"):
        if payload.get(key):
            return str(payload[key])
    source_id = _source_id(payload)
    return f"{source}:{source_id}" if source_id else None


def _freshness(payload: dict[str, Any]) -> ReviewFreshness:
    value = payload.get("freshness")
    if value in {"fresh", ReviewFreshness.FRESH.value}:
        return ReviewFreshness.FRESH
    if value in {"stale", ReviewFreshness.STALE.value} or payload.get("index_stale"):
        return ReviewFreshness.STALE
    return ReviewFreshness.UNKNOWN


def _trust_level(source: str) -> ReviewTrustLevel:
    if source in {"validation", "patch_validation", "edit_result", "verification", "verification_result", "policy_observation"}:
        return ReviewTrustLevel.TRUSTED_RUNTIME
    if source in {"code_impact", "test_impact", "project_index"}:
        return ReviewTrustLevel.WORKSPACE_DERIVED
    if source == "model_critic":
        return ReviewTrustLevel.MODEL_DERIVED
    return ReviewTrustLevel.UNKNOWN


def _sensitive_key(key: str) -> bool:
    normalized = key.lower().replace("-", "_")
    return any(part in normalized for part in SENSITIVE_KEYS)
