from __future__ import annotations

import json

from singularity.context.models import ContextItem, ContextSummaryPayload


class ContextSummaryValidationError(ValueError):
    pass


class ContextCompressor:
    REQUIRED_KEYS = {
        "goal",
        "current_state",
        "completed_actions",
        "pending_actions",
        "verified_facts",
        "failed_attempts",
        "policy_constraints",
        "workspace_changes",
        "verification_status",
        "open_questions",
        "reference_ids",
        "omitted_item_ids",
        "confidence",
    }

    def parse_summary(
        self,
        content: str,
        *,
        source_items: list[ContextItem],
        previous_summary: ContextSummaryPayload | None = None,
    ) -> ContextSummaryPayload:
        try:
            payload = json.loads(content)
        except json.JSONDecodeError as exc:
            raise ContextSummaryValidationError("context_summary_invalid_json") from exc
        if not isinstance(payload, dict):
            raise ContextSummaryValidationError("context_summary_not_object")
        missing = sorted(self.REQUIRED_KEYS - set(payload))
        if missing:
            raise ContextSummaryValidationError(
                f"context_summary_missing_keys: {', '.join(missing)}"
            )
        summary = ContextSummaryPayload(
            goal=str(payload["goal"]),
            current_state=str(payload["current_state"]),
            completed_actions=list(payload["completed_actions"] or []),
            pending_actions=list(payload["pending_actions"] or []),
            verified_facts=list(payload["verified_facts"] or []),
            failed_attempts=list(payload["failed_attempts"] or []),
            policy_constraints=[str(item) for item in (payload["policy_constraints"] or [])],
            workspace_changes=list(payload["workspace_changes"] or []),
            verification_status=str(payload["verification_status"]),
            open_questions=list(payload["open_questions"] or []),
            reference_ids=[str(item) for item in (payload["reference_ids"] or [])],
            omitted_item_ids=[str(item) for item in (payload["omitted_item_ids"] or [])],
            confidence=float(payload["confidence"]),
        )
        self._validate_verified_facts(summary)
        self._validate_omitted_items(summary, source_items)
        self._validate_drift(summary, previous_summary)
        return summary

    def _validate_verified_facts(self, summary: ContextSummaryPayload) -> None:
        for fact in summary.verified_facts:
            if isinstance(fact, str):
                raise ContextSummaryValidationError(
                    "context_summary_verified_fact_missing_reference"
                )
            if not isinstance(fact, dict):
                raise ContextSummaryValidationError("context_summary_verified_fact_invalid")
            refs = fact.get("reference_ids") or []
            if not refs:
                raise ContextSummaryValidationError(
                    "context_summary_verified_fact_missing_reference"
                )
            missing = [str(ref) for ref in refs if str(ref) not in summary.reference_ids]
            if missing:
                raise ContextSummaryValidationError(
                    f"context_summary_unknown_reference: {', '.join(missing)}"
                )

    @staticmethod
    def _validate_omitted_items(
        summary: ContextSummaryPayload,
        source_items: list[ContextItem],
    ) -> None:
        source_ids = {item.item_id for item in source_items}
        unknown = [item_id for item_id in summary.omitted_item_ids if source_ids and item_id not in source_ids]
        if unknown:
            raise ContextSummaryValidationError(
                f"context_summary_unknown_omitted_items: {', '.join(unknown)}"
            )

    @staticmethod
    def _validate_drift(
        summary: ContextSummaryPayload,
        previous_summary: ContextSummaryPayload | None,
    ) -> None:
        if previous_summary is None:
            return
        prior_constraints = set(previous_summary.policy_constraints)
        current_constraints = set(summary.policy_constraints)
        if not prior_constraints.issubset(current_constraints):
            missing = sorted(prior_constraints - current_constraints)
            raise ContextSummaryValidationError(
                f"context_summary_policy_constraint_drift: {', '.join(missing)}"
            )


def summary_to_text(summary: ContextSummaryPayload) -> str:
    return json.dumps(summary.__dict__, ensure_ascii=False, sort_keys=True, default=str)

