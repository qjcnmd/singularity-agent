from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.failure_analysis._shared import (
    FAILURE_CATEGORY_PATTERN,
    MIN_REPAIR_CONFIDENCE,
    _append_unique,
)
from singularity.failure_analysis.result import FailureAnalysisResult, _validate_verification_plan
from singularity.verification.contract import VerificationContract

BLOCKED_FAILURE_CATEGORIES = {
    "action_not_allowed",
    "approval",
    "approval_denied",
    "approval_required",
    "failure_analysis_invalid_json",
    "failure_analysis_schema_invalid",
    "failure_analysis_unavailable",
    "low_confidence",
    "missing_information",
    "permission_denied",
    "policy",
    "policy_ask_user_required",
    "policy_blocked",
    "policy_denied",
    "policy_escalation_required",
    "risk_escalated",
    "sandbox",
    "sandbox_capability_failed",
    "sandbox_required",
    "sandbox_violation",
    "user_input_required",
    "verification_failed",
}
ACTION_TYPES = {"inspect", "analyze", "edit", "verify", "ask_user"}
TOOL_HINTS = {
    "apply_patch",
    "get_verification_result",
    "inspect_diff",
    "read_file",
    "run_verification",
    "search_text",
    "workspace_health",
    "write_file",
}


@dataclass(frozen=True)
class RepairActionCandidate:
    candidate_id: str
    action_type: str
    target_file: str | None
    rationale: str
    tool_hints: list[str]
    verification_ref: str | None = None
    confidence: float = 0.5

    def to_dict(self) -> dict[str, Any]:
        return {
            "candidate_id": self.candidate_id,
            "action_type": self.action_type,
            "target_file": self.target_file,
            "rationale": self.rationale,
            "tool_hints": self.tool_hints,
            "verification_ref": self.verification_ref,
            "confidence": self.confidence,
        }


@dataclass(frozen=True)
class RepairContract:
    contract_id: str
    analysis_id: str
    failure_category: str
    target_files: list[str]
    evidence_refs: list[str]
    action_candidates: list[RepairActionCandidate]
    verification_plan: list[str]
    confidence: float
    allowed_tool_names: list[str]
    needs_user_input: bool = False
    blocked_reason: str | None = None
    validation_errors: list[str] = field(default_factory=list)
    verification_contract: VerificationContract = field(
        default_factory=VerificationContract.empty
    )

    @classmethod
    def from_analysis(
        cls,
        analysis: FailureAnalysisResult,
        *,
        action_candidates: list[RepairActionCandidate],
    ) -> RepairContract:
        allowed = _allowed_tools_from_candidates(action_candidates)
        if analysis.verification_plan or analysis.verification_contract.steps:
            allowed.extend(["run_verification", "get_verification_result"])
        errors = _repair_contract_errors(
            analysis=analysis,
            action_candidates=action_candidates,
            allowed_tool_names=allowed,
            verification_contract=analysis.verification_contract,
        )
        return cls(
            contract_id=f"repair_contract_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            failure_category=analysis.failure_category,
            target_files=list(analysis.affected_files),
            evidence_refs=list(analysis.evidence_refs),
            action_candidates=list(action_candidates),
            verification_plan=list(analysis.verification_plan),
            confidence=analysis.confidence,
            allowed_tool_names=sorted(dict.fromkeys(allowed)),
            needs_user_input=analysis.needs_user_input or bool(errors) or bool(analysis.blocked_reason),
            blocked_reason=analysis.blocked_reason or ("; ".join(errors) if errors else None),
            validation_errors=errors,
            verification_contract=analysis.verification_contract,
        )

    @classmethod
    def blocked(cls, analysis: FailureAnalysisResult, *, reason: str) -> RepairContract:
        return cls(
            contract_id=f"repair_contract_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            failure_category=analysis.failure_category,
            target_files=list(analysis.affected_files),
            evidence_refs=list(analysis.evidence_refs),
            action_candidates=[],
            verification_plan=list(analysis.verification_plan),
            confidence=analysis.confidence,
            allowed_tool_names=[],
            needs_user_input=True,
            blocked_reason=reason,
            validation_errors=[reason],
            verification_contract=analysis.verification_contract,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "contract_id": self.contract_id,
            "analysis_id": self.analysis_id,
            "failure_category": self.failure_category,
            "target_files": self.target_files,
            "evidence_refs": self.evidence_refs,
            "action_candidates": [item.to_dict() for item in self.action_candidates],
            "verification_plan": self.verification_plan,
            "verification_contract": self.verification_contract.to_dict(),
            "confidence": self.confidence,
            "allowed_tool_names": self.allowed_tool_names,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "validation_errors": self.validation_errors,
        }


def _repair_contract_errors(
    *,
    analysis: FailureAnalysisResult,
    action_candidates: list[RepairActionCandidate],
    allowed_tool_names: list[str],
    verification_contract: VerificationContract | None = None,
) -> list[str]:
    errors: list[str] = []
    if not FAILURE_CATEGORY_PATTERN.match(analysis.failure_category):
        errors.append("invalid_failure_category")
    if analysis.confidence < MIN_REPAIR_CONFIDENCE:
        errors.append("low_confidence")
    if not analysis.needs_user_input and not analysis.evidence_refs:
        errors.append("missing_evidence_refs")
    if not analysis.needs_user_input and not analysis.affected_files:
        errors.append("missing_target_files")
    if not analysis.needs_user_input and not action_candidates:
        errors.append("missing_action_candidates")
    for candidate in action_candidates:
        errors.extend(_repair_action_candidate_errors(candidate, target_files=analysis.affected_files))
    if not analysis.needs_user_input:
        try:
            _validate_verification_plan(
                analysis.verification_plan,
                needs_user_input=False,
                verification_contract=verification_contract,
            )
        except ValueError as exc:
            errors.append(str(exc))
    if verification_contract is not None and verification_contract.validation_errors:
        errors.extend(verification_contract.validation_errors)
    if any(tool not in TOOL_HINTS for tool in allowed_tool_names):
        errors.append("unsupported_tool_hint")
    return sorted(dict.fromkeys(errors))


def _repair_action_candidate_errors(
    candidate: RepairActionCandidate,
    *,
    target_files: list[str],
) -> list[str]:
    errors: list[str] = []
    if candidate.action_type not in ACTION_TYPES:
        errors.append(f"invalid_action_type:{candidate.action_type}")
    if not candidate.rationale.strip():
        errors.append("missing_action_rationale")
    if candidate.confidence < 0.0 or candidate.confidence > 1.0:
        errors.append("invalid_action_confidence")
    for tool in candidate.tool_hints:
        if tool not in TOOL_HINTS:
            errors.append(f"unsupported_tool_hint:{tool}")
    if candidate.action_type in {"edit", "inspect"} and not candidate.target_file:
        errors.append("missing_target_file")
    if candidate.target_file and target_files and candidate.target_file not in target_files:
        errors.append(f"unauthorized_candidate_target:{candidate.target_file}")
    return errors


def _allowed_tools_from_candidates(candidates: list[RepairActionCandidate]) -> list[str]:
    allowed: list[str] = []
    for candidate in candidates:
        for tool in candidate.tool_hints:
            _append_unique(allowed, tool)
    return allowed
