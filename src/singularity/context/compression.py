from __future__ import annotations

import json
from typing import Any

from singularity.context.models import (
    CacheAttribution,
    ContextItem,
    ContextSummaryEnvelope,
    ContextSummaryPayload,
    digest_value,
)


class ContextSummaryValidationError(ValueError):
    pass


class ContextCompressor:
    SUMMARY_VERSION = 1
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
        previous_summary: ContextSummaryPayload | ContextSummaryEnvelope | None = None,
    ) -> ContextSummaryPayload:
        envelope = self.parse_envelope(
            content,
            source_items=source_items,
            previous_summary=previous_summary,
        )
        if envelope.summary_payload is None:
            raise ContextSummaryValidationError("context_summary_missing_payload")
        return envelope.summary_payload

    def parse_envelope(
        self,
        content: str,
        *,
        source_items: list[ContextItem],
        previous_summary: ContextSummaryPayload | ContextSummaryEnvelope | None = None,
    ) -> ContextSummaryEnvelope:
        try:
            payload = json.loads(content)
        except json.JSONDecodeError as exc:
            raise ContextSummaryValidationError("context_summary_invalid_json") from exc
        if not isinstance(payload, dict):
            raise ContextSummaryValidationError("context_summary_not_object")
        envelope = self._coerce_envelope(payload)
        self._validate_envelope(envelope, source_items=source_items, previous_summary=previous_summary)
        return envelope

    def _coerce_envelope(self, payload: dict[str, Any]) -> ContextSummaryEnvelope:
        if self._looks_like_envelope(payload):
            envelope = ContextSummaryEnvelope.from_dict(payload)
            if envelope.summary_payload is None:
                raise ContextSummaryValidationError("context_summary_missing_payload")
            return envelope
        summary = self._parse_summary_payload(payload)
        return ContextSummaryEnvelope(
            version=self.SUMMARY_VERSION,
            summary_id=str(payload.get("summary_id") or payload.get("id") or ""),
            summary_payload=summary,
            source_item_ids=[str(item) for item in summary.omitted_item_ids],
            cache_attribution=CacheAttribution.from_dict(
                payload.get("cache_attribution") if isinstance(payload.get("cache_attribution"), dict) else {}
            ),
            previous_summary_digest=payload.get("previous_summary_digest"),
            rendered_summary=str(
                payload.get("rendered_summary")
                or payload.get("summary")
                or summary.current_state
            ),
            metadata=dict(payload.get("metadata") or {}),
        )

    def _parse_summary_payload(self, payload: dict[str, Any]) -> ContextSummaryPayload:
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
        return summary

    def _validate_envelope(
        self,
        envelope: ContextSummaryEnvelope,
        *,
        source_items: list[ContextItem],
        previous_summary: ContextSummaryPayload | ContextSummaryEnvelope | None,
    ) -> None:
        summary = envelope.summary_payload
        if summary is None:
            raise ContextSummaryValidationError("context_summary_missing_payload")
        self._validate_verified_facts(summary)
        self._validate_omitted_items(summary, source_items)
        prior = self._previous_summary_payload(previous_summary)
        self._validate_drift(summary, prior)
        if envelope.previous_summary_digest and prior is not None:
            actual = digest_value(prior)
            if envelope.previous_summary_digest != actual:
                raise ContextSummaryValidationError(
                    "context_summary_previous_summary_digest_mismatch"
                )
        if envelope.summary_payload is not None and envelope.summary_digest:
            actual_summary_digest = digest_value(summary)
            if envelope.summary_digest != actual_summary_digest:
                raise ContextSummaryValidationError(
                    "context_summary_digest_mismatch"
                )
        if envelope.version != self.SUMMARY_VERSION:
            raise ContextSummaryValidationError(
                f"context_summary_unsupported_version: {envelope.version}"
            )
        if not envelope.rendered_summary:
            envelope.rendered_summary = summary_to_text(summary)

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
        _require_prior_subset(
            previous_summary.verified_facts,
            summary.verified_facts,
            "context_summary_verified_fact_drift",
        )
        _require_prior_subset(
            previous_summary.workspace_changes,
            summary.workspace_changes,
            "context_summary_workspace_change_drift",
        )
        _require_prior_subset(
            previous_summary.failed_attempts,
            summary.failed_attempts,
            "context_summary_failed_attempt_drift",
        )
        prior_status = previous_summary.verification_status
        current_status = summary.verification_status
        unknown = {"", "unknown", "not_run", "none"}
        if prior_status not in unknown and current_status in unknown:
            raise ContextSummaryValidationError(
                "context_summary_verification_status_drift"
            )

    @staticmethod
    def _previous_summary_payload(
        previous_summary: ContextSummaryPayload | ContextSummaryEnvelope | None,
    ) -> ContextSummaryPayload | None:
        if previous_summary is None:
            return None
        if isinstance(previous_summary, ContextSummaryEnvelope):
            return previous_summary.summary_payload
        return previous_summary

    @staticmethod
    def _looks_like_envelope(payload: dict[str, Any]) -> bool:
        return any(
            key in payload
            for key in (
                "summary_payload",
                "rendered_summary",
                "previous_summary_digest",
                "summary_id",
                "version",
                "cache_attribution",
            )
        )


def _require_prior_subset(previous: list[object], current: list[object], code: str) -> None:
    previous_values = {_stable_value(item) for item in previous}
    current_values = {_stable_value(item) for item in current}
    missing = sorted(previous_values - current_values)
    if missing:
        raise ContextSummaryValidationError(f"{code}: {', '.join(missing[:5])}")


def _stable_value(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)


def summary_to_text(summary: ContextSummaryPayload) -> str:
    return json.dumps(summary.__dict__, ensure_ascii=False, sort_keys=True, default=str)

