from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.verification.contract import INTERNAL_VERIFICATION_REFS, VerificationContract

from ._shared import (
    FAILURE_CATEGORY_PATTERN,
    MIN_REPAIR_CONFIDENCE,
    SUMMARY_LIMIT,
    _append_unique,
    _limit,
    _normalize_workspace_path,
    _strings,
    _text,
)
from .request import FailureAnalysisRequest


def _json_payload(text: str) -> dict[str, Any]:
    """Parse a JSON object from model text via ``OutputParser``.

    Legacy wrapper — prefer ``OutputParser().parse()`` directly.
    Preserves the old return/raise contract. Uses lazy import to
    avoid circular dependency through model → tools → repair → result.
    """
    from singularity.model.output import OutputParser  # lazy — avoid circular import

    result = OutputParser().parse(text)
    if not result.ok:
        raise ValueError(
            result.errors[0].message if result.errors else "parse failed"
        )
    return result.parsed  # type: ignore[return-value]


@dataclass(frozen=True)
class FailureAnalysisResult:
    analysis_id: str
    request_id: str
    root_cause: str
    failure_category: str
    affected_files: list[str]
    evidence_refs: list[str]
    repair_strategy: str
    next_actions: list[str]
    verification_plan: list[str]
    confidence: float
    needs_user_input: bool
    blocked_reason: str | None = None
    raw_response_ref: str | None = None
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    @classmethod
    def from_model_payload(
        cls,
        payload: dict[str, Any],
        *,
        request: FailureAnalysisRequest,
        raw_response_ref: str | None = None,
    ) -> FailureAnalysisResult:
        needs_user_input = _bool_required(payload, "needs_user_input")
        confidence = _confidence_required(payload.get("confidence"))
        category = _required_text(payload, "failure_category")
        category = category.replace("/", "_").replace("-", "_")
        if not FAILURE_CATEGORY_PATTERN.match(category):
            raise ValueError(f"invalid failure_category: {category!r}")
        root_cause = _required_text(payload, "root_cause")
        evidence_refs = _strings_required(payload, "evidence_refs")
        _validate_evidence_refs(evidence_refs, request=request)
        affected = _validated_affected_files(payload.get("affected_files"), request=request)
        next_actions = _strings_required(payload, "next_actions")
        verification_plan = _strings_required(payload, "verification_plan") if not needs_user_input else _strings(
            payload.get("verification_plan")
        )
        verification_contract = VerificationContract.from_plan_strings(verification_plan)
        _validate_verification_plan(
            verification_plan,
            needs_user_input=needs_user_input,
            verification_contract=verification_contract,
        )
        blocked_reason = _text(payload.get("blocked_reason")) or None
        if needs_user_input and not blocked_reason:
            raise ValueError("blocked_reason is required when needs_user_input=true")
        if not needs_user_input and confidence < MIN_REPAIR_CONFIDENCE:
            raise ValueError(f"confidence below repair threshold: {confidence}")
        if not needs_user_input and not affected:
            raise ValueError("affected_files must identify at least one workspace target")
        return cls(
            analysis_id=str(payload.get("analysis_id") or f"failure_{uuid4().hex[:12]}"),
            request_id=request.request_id,
            root_cause=root_cause,
            failure_category=category,
            affected_files=affected[:20],
            evidence_refs=evidence_refs[:30],
            repair_strategy=_required_text(payload, "repair_strategy"),
            next_actions=next_actions[:12],
            verification_plan=verification_plan[:12],
            confidence=confidence,
            needs_user_input=needs_user_input,
            blocked_reason=blocked_reason,
            raw_response_ref=raw_response_ref,
            verification_contract=verification_contract,
        )

    @classmethod
    def blocked(
        cls,
        *,
        request: FailureAnalysisRequest,
        reason: str,
        category: str = "failure_analysis_unavailable",
        affected_files: list[str] | None = None,
    ) -> FailureAnalysisResult:
        return cls(
            analysis_id=f"failure_{uuid4().hex[:12]}",
            request_id=request.request_id,
            root_cause=reason,
            failure_category=category,
            affected_files=list(request.allowed_target_files if affected_files is None else affected_files),
            evidence_refs=list(request.evidence_refs),
            repair_strategy="blocked",
            next_actions=[reason],
            verification_plan=[],
            confidence=0.0,
            needs_user_input=True,
            blocked_reason=reason,
        )

    def to_dict(self) -> dict[str, Any]:
        root = {
            "description": self.root_cause,
            "evidence": self.evidence_refs,
            "confidence": self.confidence,
        }
        return {
            "analysis_id": self.analysis_id,
            "request_id": self.request_id,
            "root_cause": root,
            "failure_category": self.failure_category,
            "affected_files": self.affected_files,
            "evidence_refs": self.evidence_refs,
            "repair_strategy": self.repair_strategy,
            "next_actions": self.next_actions,
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "raw_response_ref": self.raw_response_ref,
        }

def _json_payload(text: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        match = re.search(r"\{.*\}", text, flags=re.DOTALL)
        if not match:
            raise ValueError("model response did not contain a JSON object") from None
        value = json.loads(match.group(0))
    if not isinstance(value, dict):
        raise ValueError("model response JSON was not an object")
    return value


def _required_text(payload: dict[str, Any], field_name: str) -> str:
    value = payload.get(field_name)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field_name} must be a non-empty string")
    return _limit(value.strip(), SUMMARY_LIMIT)


def _bool_required(payload: dict[str, Any], field_name: str) -> bool:
    value = payload.get(field_name)
    if not isinstance(value, bool):
        raise ValueError(f"{field_name} must be a boolean")
    return value


def _strings_required(payload: dict[str, Any], field_name: str) -> list[str]:
    if field_name not in payload:
        raise ValueError(f"{field_name} is required")
    values = _strings(payload.get(field_name))
    if not values:
        raise ValueError(f"{field_name} must contain at least one item")
    return [_limit(item.strip(), SUMMARY_LIMIT) for item in values if item.strip()]


def _confidence_required(value: Any) -> float:
    if isinstance(value, bool):
        raise ValueError("confidence must be numeric")
    try:
        confidence = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError("confidence must be numeric") from exc
    if confidence < 0.0 or confidence > 1.0:
        raise ValueError("confidence must be between 0 and 1")
    return confidence


def _validate_evidence_refs(refs: list[str], *, request: FailureAnalysisRequest) -> None:
    known = set(request.evidence_refs) | set(request.context_references) | set(request.verification_log_refs)
    known.update(request.failure_evidence_refs)
    if not refs:
        raise ValueError("evidence_refs must not be empty")
    if known and not any(ref in known for ref in refs):
        raise ValueError("evidence_refs must reference supplied failure evidence")


def _validated_affected_files(value: Any, *, request: FailureAnalysisRequest) -> list[str]:
    raw_paths = _strings(value)
    allowed = {path for path in request.allowed_target_files if path}
    if not raw_paths:
        return []
    resolved: list[str] = []
    for raw_path in raw_paths:
        normalized = _normalize_workspace_path(raw_path, workspace_root=request.workspace_root)
        if not normalized:
            raise ValueError(f"affected_files contains an invalid workspace path: {raw_path}")
        if allowed and normalized not in allowed:
            raise ValueError(f"affected_files contains unauthorized target: {raw_path}")
        _append_unique(resolved, normalized)
    return resolved


def _validate_verification_plan(
    plan: list[str],
    *,
    needs_user_input: bool,
    verification_contract: VerificationContract | None = None,
) -> None:
    if needs_user_input:
        return
    has_contract = (
        verification_contract is not None
        and verification_contract.is_valid
    )
    if not plan and not has_contract:
        raise ValueError("verification_plan must contain at least one executable verification step")
    if verification_contract is not None and verification_contract.validation_errors:
        raise ValueError(
            "verification_contract invalid: " + "; ".join(verification_contract.validation_errors)
        )
    for item in plan:
        text = item.strip()
        if not text:
            raise ValueError("verification_plan contains an empty step")
        if text in INTERNAL_VERIFICATION_REFS:
            continue
        if len(text.split()) < 2:
            raise ValueError(f"verification_plan step is not executable enough: {text}")
