from __future__ import annotations

from miniharness.memory.models import (
    Confidence,
    MemoryCandidate,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryType,
    Provenance,
)
from miniharness.memory.policy import AdmissionAction, MemoryPolicy


def candidate(
    *,
    body: str,
    source: MemorySource = MemorySource.FINAL_REPORT,
    scope: MemoryScope = MemoryScope.PROJECT,
    type: MemoryType = MemoryType.LESSON,
    evidence: list[MemoryEvidenceRef] | None = None,
    confidence: Confidence = Confidence.MEDIUM,
) -> MemoryCandidate:
    return MemoryCandidate(
        id="cand_1",
        scope=scope,
        type=type,
        source=source,
        title="Candidate",
        body=body,
        confidence=confidence,
        provenance=Provenance(evidence=evidence or []),
    )


def test_policy_requires_stable_evidence_for_long_term_memory() -> None:
    decision = MemoryPolicy().evaluate(candidate(body="Model guessed this may be true."))

    assert decision.action == AdmissionAction.QUARANTINE
    assert "stable evidence" in " ".join(decision.reasons)


def test_policy_redacts_secret_like_content_before_admission() -> None:
    evidence = [
        MemoryEvidenceRef(source=MemorySource.VERIFICATION, ref_id="check", summary="verified")
    ]
    decision = MemoryPolicy().evaluate(
        candidate(body="Use API_KEY=sk-secret and password=hunter2.", evidence=evidence)
    )

    assert decision.action == AdmissionAction.ACCEPT
    assert "sk-secret" not in decision.candidate.body
    assert "hunter2" not in decision.candidate.body
    assert "[REDACTED]" in decision.candidate.body


def test_policy_rejects_temporary_paths_and_one_time_state() -> None:
    evidence = [
        MemoryEvidenceRef(source=MemorySource.TRACE, ref_id="trace", summary="observed once")
    ]
    decision = MemoryPolicy().evaluate(
        candidate(body=r"Current tmp path is C:\Users\Lenovo\AppData\Local\Temp\tmp123.", evidence=evidence)
    )

    assert decision.action == AdmissionAction.REJECT
    assert any("temporary" in reason for reason in decision.reasons)


def test_policy_quarantines_model_only_failure_guess() -> None:
    evidence = [
        MemoryEvidenceRef(source=MemorySource.MODEL, ref_id="turn_1", summary="assistant guess")
    ]
    decision = MemoryPolicy().evaluate(
        candidate(
            body="Failure was probably caused by a race condition.",
            source=MemorySource.MODEL,
            evidence=evidence,
        )
    )

    assert decision.action == AdmissionAction.QUARANTINE
    assert any("guess" in reason or "model-only" in reason for reason in decision.reasons)


def test_user_preference_requires_explicit_user_or_manual_acceptance() -> None:
    evidence = [
        MemoryEvidenceRef(source=MemorySource.FINAL_REPORT, ref_id="report", summary="summary")
    ]
    decision = MemoryPolicy().evaluate(
        candidate(
            body="User prefers terse responses.",
            source=MemorySource.FINAL_REPORT,
            scope=MemoryScope.USER_PREFERENCE,
            type=MemoryType.USER_PREFERENCE,
            evidence=evidence,
        )
    )
    explicit = MemoryPolicy().evaluate(
        candidate(
            body="User explicitly prefers terse responses.",
            source=MemorySource.USER,
            scope=MemoryScope.USER_PREFERENCE,
            type=MemoryType.USER_PREFERENCE,
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.USER,
                    ref_id="user_message",
                    summary="explicit preference",
                )
            ],
        )
    )

    assert decision.action == AdmissionAction.QUARANTINE
    assert explicit.action == AdmissionAction.ACCEPT


def test_user_preference_can_be_accepted_with_manual_evidence() -> None:
    decision = MemoryPolicy().evaluate(
        candidate(
            body="User prefers concise Chinese responses.",
            source=MemorySource.FINAL_REPORT,
            scope=MemoryScope.USER_PREFERENCE,
            type=MemoryType.USER_PREFERENCE,
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.MANUAL,
                    ref_id="manual_accept:cand_pref",
                    summary="manual accept",
                )
            ],
        )
    )

    assert decision.action == AdmissionAction.ACCEPT
