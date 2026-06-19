from miniharness.review.decision import ReviewDecisionEngine
from miniharness.review.evidence import collect_review_evidence, stable_payload_hash, to_bounded_plain
from miniharness.review.exceptions import ReviewCriticError, ReviewRuntimeError
from miniharness.review.findings import RuleFindingCollector
from miniharness.review.models import (
    ReviewCategory,
    ReviewDecision,
    ReviewDecisionAction,
    ReviewEvidence,
    ReviewFinding,
    ReviewFreshness,
    ReviewLocation,
    ReviewReport,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
    ReviewTrustLevel,
)

__all__ = [
    "ModelCritic",
    "ModelCriticOutcome",
    "ReviewCategory",
    "ReviewCriticError",
    "ReviewDecision",
    "ReviewDecisionAction",
    "ReviewDecisionEngine",
    "ReviewEvidence",
    "ReviewFinding",
    "ReviewFreshness",
    "ReviewLocation",
    "ReviewReport",
    "ReviewRuntime",
    "ReviewRuntimeError",
    "ReviewSeverity",
    "ReviewStage",
    "ReviewTarget",
    "ReviewTrustLevel",
    "RuleFindingCollector",
    "collect_review_evidence",
    "stable_payload_hash",
    "to_bounded_plain",
]


def __getattr__(name: str):
    if name in {"ModelCritic", "ModelCriticOutcome"}:
        from miniharness.review.critic import ModelCritic, ModelCriticOutcome

        return {"ModelCritic": ModelCritic, "ModelCriticOutcome": ModelCriticOutcome}[name]
    if name == "ReviewRuntime":
        from miniharness.review.runtime import ReviewRuntime

        return ReviewRuntime
    raise AttributeError(name)
