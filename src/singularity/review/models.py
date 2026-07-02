from __future__ import annotations

import hashlib
import json
from datetime import UTC, datetime
from enum import StrEnum
from typing import Any
from uuid import uuid4

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class ReviewStage(StrEnum):
    PRE_EDIT = "pre_edit"
    POST_PATCH = "post_patch"
    POST_VERIFICATION = "post_verification"
    FINAL = "final"


class ReviewSeverity(StrEnum):
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    CRITICAL = "critical"


class ReviewCategory(StrEnum):
    GOAL_MISMATCH = "goal_mismatch"
    OVER_EDITING = "over_editing"
    BUG_RISK = "bug_risk"
    TEST_GAP = "test_gap"
    ARCHITECTURE_REGRESSION = "architecture_regression"
    SECURITY_RISK = "security_risk"
    MAINTAINABILITY = "maintainability"
    STYLE = "style"
    VERIFICATION_GAP = "verification_gap"
    POLICY_RISK = "policy_risk"


class ReviewDecisionAction(StrEnum):
    ACCEPT = "accept"
    REPAIR = "repair"
    REPLAN = "replan"
    ROLLBACK = "rollback"
    NEEDS_HUMAN_APPROVAL = "needs_human_approval"


class ReviewTrustLevel(StrEnum):
    TRUSTED_COMPONENT = "trusted_component"
    TRUSTED_OPERATOR = "trusted_operator"
    WORKSPACE_DERIVED = "workspace_derived"
    MODEL_DERIVED = "model_derived"
    UNTRUSTED_WORKSPACE_DATA = "untrusted_workspace_data"
    UNKNOWN = "unknown"


class ReviewFreshness(StrEnum):
    FRESH = "fresh"
    STALE = "stale"
    UNKNOWN = "unknown"


class ReviewTarget(BaseModel):
    model_config = ConfigDict(extra="forbid")

    stage: ReviewStage
    task_id: str | None = None
    plan_id: str | None = None
    edit_intent_id: str | None = None
    edit_plan_id: str | None = None
    patch_id: str | None = None
    patch_digest: str | None = None
    edit_result_id: str | None = None
    verification_id: str | None = None
    policy_decision_id: str | None = None
    trace_id: str | None = None
    changeset_id: str | None = None
    transaction_id: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)

    def reference_ids(self) -> dict[str, str]:
        return {
            key: value
            for key, value in self.model_dump(mode="json").items()
            if key.endswith("_id") and isinstance(value, str) and value
        }


class ReviewEvidence(BaseModel):
    model_config = ConfigDict(extra="forbid")

    evidence_id: str = Field(default_factory=lambda: f"review_evidence_{uuid4().hex[:12]}")
    source: str
    summary: str
    payload_hash: str
    payload: dict[str, Any] = Field(default_factory=dict)
    source_id: str | None = None
    artifact_ref: str | None = None
    freshness: ReviewFreshness = ReviewFreshness.UNKNOWN
    trust_level: ReviewTrustLevel = ReviewTrustLevel.UNKNOWN
    captured_at: str = Field(default_factory=lambda: datetime.now(UTC).isoformat())
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("payload_hash")
    @classmethod
    def hash_is_not_empty(cls, value: str) -> str:
        if not value:
            return _hash_payload({})
        return value


class ReviewLocation(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str | None = None
    lines: str | None = None
    symbol: str | None = None
    detail: str | None = None


class ReviewFinding(BaseModel):
    model_config = ConfigDict(extra="forbid")

    finding_id: str = Field(default_factory=lambda: f"review_finding_{uuid4().hex[:12]}")
    title: str
    severity: ReviewSeverity
    category: ReviewCategory
    location: ReviewLocation | dict[str, Any] | None = None
    evidence: list[str] = Field(default_factory=list)
    evidence_ids: list[str] = Field(default_factory=list)
    recommendation: str
    blocking: bool = False
    confidence: float = Field(default=0.8, ge=0.0, le=1.0)
    source: str = "rules"
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("location")
    @classmethod
    def normalize_location(cls, value: ReviewLocation | dict[str, Any] | None) -> ReviewLocation | None:
        if value is None or isinstance(value, ReviewLocation):
            return value
        return ReviewLocation(**value)


class ReviewFindings(BaseModel):
    model_config = ConfigDict(extra="forbid")

    findings: list[ReviewFinding]


class ReviewDecision(BaseModel):
    model_config = ConfigDict(extra="forbid")

    action: ReviewDecisionAction = ReviewDecisionAction.ACCEPT
    route: str | None = None
    reasons: list[str] = Field(default_factory=list)
    finding_ids: list[str] = Field(default_factory=list)
    confidence: float = Field(default=0.9, ge=0.0, le=1.0)
    next_actions: list[str] = Field(default_factory=list)
    repair_targets: list[str] = Field(default_factory=list)
    replan_signal: dict[str, Any] = Field(default_factory=dict)
    rollback_transaction_id: str | None = None
    requires_human_approval: bool = False
    required_approval_decision_id: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="after")
    def default_route(self) -> ReviewDecision:
        if self.route is None:
            self.route = {
                ReviewDecisionAction.ACCEPT: "approve",
                ReviewDecisionAction.REPAIR: "repair",
                ReviewDecisionAction.REPLAN: "replan",
                ReviewDecisionAction.NEEDS_HUMAN_APPROVAL: "ask_user",
                ReviewDecisionAction.ROLLBACK: "blocked",
            }[self.action]
        return self


class ReviewReport(BaseModel):
    model_config = ConfigDict(extra="forbid")

    review_id: str = Field(default_factory=lambda: f"review_{uuid4().hex[:12]}")
    target: ReviewTarget
    input_summary: str
    evidence: list[ReviewEvidence] = Field(default_factory=list)
    findings: list[ReviewFinding] = Field(default_factory=list)
    decision: ReviewDecision = Field(default_factory=ReviewDecision)
    next_actions: list[str] = Field(default_factory=list)
    trace_event_ids: list[str] = Field(default_factory=list)
    model_critic_status: str = "not_run"
    model_critic_error: str | None = None
    created_at: str = Field(default_factory=lambda: datetime.now(UTC).isoformat())
    metadata: dict[str, Any] = Field(default_factory=dict)

    @property
    def blocking_findings(self) -> list[ReviewFinding]:
        return [finding for finding in self.findings if finding.blocking]


def _hash_payload(payload: dict[str, Any]) -> str:
    text = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
