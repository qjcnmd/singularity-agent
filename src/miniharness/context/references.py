from __future__ import annotations

from miniharness.context.store import ContextReference, ObservationStore


class ReferenceResolver:
    def __init__(self, store: ObservationStore) -> None:
        self.store = store

    def references_for_observation(self, observation_id: str) -> list[ContextReference]:
        return self.store.references_for_observation(observation_id)
