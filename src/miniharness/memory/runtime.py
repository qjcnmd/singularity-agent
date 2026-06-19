from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.memory.extractor import MemoryExtractor
from miniharness.memory.injector import MemoryInjector
from miniharness.memory.maintenance import MemoryMaintenance
from miniharness.memory.models import (
    MemoryCandidate,
    MemoryContextBlock,
    MemoryEvidenceRef,
    MemoryQuery,
    MemorySearchResult,
    MemorySource,
    MemoryStatus,
    Provenance,
    _now,
)
from miniharness.memory.policy import AdmissionAction, MemoryPolicy
from miniharness.memory.retrieval import MemoryRetrieval
from miniharness.memory.store import MemoryStore


class MemoryRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        store: MemoryStore | None = None,
        policy: MemoryPolicy | None = None,
        extractor: MemoryExtractor | None = None,
        trace: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.store = store or MemoryStore(self.workspace_root)
        self.policy = policy or MemoryPolicy()
        self.extractor = extractor or MemoryExtractor()
        self.maintenance = MemoryMaintenance(self.store)
        self.trace = trace
        self.session_id: str | None = None
        self.user_goal: str = ""

    def start_session(self, *, session_id: str, user_goal: str = "") -> None:
        self.session_id = session_id
        self.user_goal = user_goal
        self.store.initialize()
        self.store.rebuild_index()
        self._record("memory.session_started", {"session_id": session_id, "user_goal": user_goal})

    def ingest_candidate(self, candidate: MemoryCandidate, *, accept: bool = False) -> MemoryCandidate:
        self.store.initialize()
        decision = self.policy.evaluate(candidate)
        stored = self.store.upsert_candidate(decision.candidate)
        if accept and decision.action == AdmissionAction.ACCEPT:
            self.store.accept_candidate(stored.id)
        self._record(
            "memory.candidate_ingested",
            {
                "candidate_id": stored.id,
                "action": decision.action.value,
                "accepted": accept and decision.action == AdmissionAction.ACCEPT,
            },
        )
        return stored

    def ingest_candidates(self, candidates: list[MemoryCandidate], *, accept: bool = False) -> list[MemoryCandidate]:
        return [self.ingest_candidate(candidate, accept=accept) for candidate in candidates]

    def ingest_trace_summary(self, trace_summary: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_trace_summary(trace_summary), accept=accept)

    def ingest_final_report(self, final_report: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_final_report(final_report), accept=accept)

    def ingest_review_report(self, report: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_review_report(report), accept=accept)

    def ingest_verification_result(self, result: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_verification_result(result), accept=accept)

    def ingest_verification_observation(self, observation: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_verification_observation(observation), accept=accept)

    def ingest_rollback(self, rollback: Any, *, accept: bool = False) -> list[MemoryCandidate]:
        return self.ingest_candidates(self.extractor.from_rollback(rollback), accept=accept)

    def ingest_session_end(
        self,
        *,
        final_reports: list[Any] | None = None,
        trace_summary: Any | None = None,
        review_reports: list[Any] | None = None,
        verification_results: list[Any] | None = None,
        rollback_reasons: list[Any] | None = None,
    ) -> dict[str, int]:
        counts = {"candidates": 0}
        if trace_summary:
            counts["candidates"] += len(self.ingest_trace_summary(trace_summary))
        for report in final_reports or []:
            counts["candidates"] += len(self.ingest_final_report(report))
        for report in review_reports or []:
            counts["candidates"] += len(self.ingest_review_report(report))
        for result in verification_results or []:
            counts["candidates"] += len(self.ingest_verification_result(result))
        for rollback in rollback_reasons or []:
            counts["candidates"] += len(self.ingest_rollback(rollback))
        self.maintenance.run()
        self._record("memory.session_ingested", counts)
        return counts

    def retrieve(
        self,
        *,
        goal: str = "",
        paths: list[str] | None = None,
        tools: list[str] | None = None,
        error_types: list[str] | None = None,
        modules: list[str] | None = None,
        limit: int = 8,
    ) -> list[MemorySearchResult]:
        query = MemoryQuery(
            goal=goal,
            paths=paths or [],
            tools=tools or [],
            error_types=error_types or [],
            modules=modules or [],
            limit=limit,
        )
        return MemoryRetrieval(self.store.load_entries()).search(query)

    def context_block(
        self,
        *,
        goal: str = "",
        paths: list[str] | None = None,
        tools: list[str] | None = None,
        error_types: list[str] | None = None,
        modules: list[str] | None = None,
        max_items: int = 6,
        token_budget: int = 512,
    ) -> MemoryContextBlock:
        results = self.retrieve(
            goal=goal,
            paths=paths,
            tools=tools,
            error_types=error_types,
            modules=modules,
            limit=max_items,
        )
        return MemoryInjector(max_items=max_items, token_budget=token_budget).build_block(results)

    def accept_candidate(self, candidate_id: str):
        candidate = self.store.get_candidate(candidate_id)
        evidence = list(candidate.provenance.evidence)
        if not any(item.source == MemorySource.MANUAL for item in evidence):
            evidence.append(
                MemoryEvidenceRef(
                    source=MemorySource.MANUAL,
                    ref_id=f"manual_accept:{candidate_id}",
                    summary="Accepted through local memory control plane.",
                    captured_at=_now(),
                    trust_level="trusted_operator",
                )
            )
        payload = candidate.to_dict()
        payload["provenance"] = Provenance(
            evidence=evidence,
            created_by=candidate.provenance.created_by,
            source_run_id=candidate.provenance.source_run_id,
            source_session_id=candidate.provenance.source_session_id,
            source_task_id=candidate.provenance.source_task_id,
            extracted_at=candidate.provenance.extracted_at,
            notes=[*candidate.provenance.notes, "manual_accept"],
        ).to_dict()
        payload["updated_at"] = _now()
        candidate_with_manual_evidence = MemoryCandidate.from_dict(payload)
        decision = self.policy.evaluate(candidate_with_manual_evidence)
        if decision.action != AdmissionAction.ACCEPT:
            self.store.upsert_candidate(decision.candidate)
            raise ValueError(
                f"Memory candidate {candidate_id} remains quarantined: "
                + "; ".join(decision.reasons)
            )
        self.store.upsert_candidate(decision.candidate)
        return self.store.accept_candidate(candidate_id)

    def reject_candidate(self, candidate_id: str, *, reason: str = "rejected"):
        return self.store.reject_candidate(candidate_id, reason=reason)

    def delete_entry(self, entry_id: str, *, reason: str = "deleted"):
        return self.maintenance.delete(entry_id, reason=reason)

    def refresh(self) -> dict[str, Any]:
        return self.maintenance.refresh()

    def doctor(self, *, repair: bool = False) -> dict[str, Any]:
        self.store.initialize()
        return self.maintenance.doctor(repair=repair)

    def health(self) -> dict[str, Any]:
        self.store.initialize()
        entries = self.store.load_entries()
        candidates = self.store.load_candidates()
        return {
            "status": "ok",
            "workspace_root": str(self.workspace_root),
            "memory_root": str(self.store.root),
            "entries": len(entries),
            "active_entries": len([entry for entry in entries if entry.status == MemoryStatus.ACTIVE]),
            "candidates": len(candidates),
        }

    def _record(self, event: str, payload: dict[str, Any]) -> None:
        if self.trace is None:
            return
        if hasattr(self.trace, "record"):
            self.trace.record(event, payload)
