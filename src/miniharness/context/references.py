from __future__ import annotations

from miniharness.context.models import ContextFreshness, ContextReference
from miniharness.context.store import ObservationStore


class ReferenceResolver:
    def __init__(self, store: ObservationStore) -> None:
        self.store = store

    def resolve(self, ref_id: str) -> ContextReference | None:
        return self.store.resolve_reference(ref_id)

    def resolve_many(self, ref_ids: list[str]) -> list[ContextReference]:
        return [
            reference
            for ref_id in ref_ids
            if (reference := self.resolve(ref_id)) is not None
        ]

    def references_for_observation(self, observation_id: str) -> list[ContextReference]:
        return self.store.references_for_observation(observation_id)

    def references_for_file(self, path: str) -> list[ContextReference]:
        return self.store.references_for_target(path, ref_type="file")

    def references_for_transaction(self, transaction_id: str) -> list[ContextReference]:
        return self.store.references_for_target(transaction_id, ref_type="transaction")

    def references_for_policy_decision(self, decision_id: str) -> list[ContextReference]:
        return self.store.references_for_target(decision_id, ref_type="policy_decision")

    def references_for_verification(self, check_id: str) -> list[ContextReference]:
        return self.store.references_for_target(check_id, ref_type="verification")

    def validate_reference_freshness(self, ref_id: str) -> bool:
        reference = self.resolve(ref_id)
        return bool(reference and reference.freshness == ContextFreshness.CURRENT)

    def mark_references_stale_for_path(self, path: str, *, reason: str = "") -> None:
        for reference in self.references_for_file(path):
            self.store.update_reference_freshness(
                reference.ref_id,
                ContextFreshness.STALE,
                reason=reason,
            )

    def render_reference_for_model(self, ref_id: str) -> str:
        reference = self.resolve(ref_id)
        if reference is None:
            return f"[{ref_id}] missing reference"
        location = reference.path or reference.target or "<unknown>"
        line_suffix = ""
        if reference.line_start is not None:
            if reference.line_end and reference.line_end != reference.line_start:
                line_suffix = f":{reference.line_start}-{reference.line_end}"
            else:
                line_suffix = f":{reference.line_start}"
        digest = f" digest={reference.digest}" if reference.digest else ""
        return f"[{reference.ref_id}] {reference.ref_type} {location}{line_suffix}{digest}"
