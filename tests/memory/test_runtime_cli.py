from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.memory.models import (
    MemoryCandidate,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryType,
    Provenance,
)
from miniharness.memory.runtime import MemoryRuntime


runner = CliRunner()


def test_runtime_start_session_ingests_candidates_and_retrieves_context(tmp_path: Path) -> None:
    runtime = MemoryRuntime(tmp_path)
    runtime.start_session(session_id="session_1", user_goal="memory runtime")
    runtime.ingest_verification_result(
        {
            "check_id": "check_1",
            "kind": "unit_test",
            "status": "passed",
            "evidence": {
                "command": "python -m pytest tests/memory --basetemp work/pytest-tmp",
                "output_excerpt": "passed",
            },
        },
        accept=True,
    )

    block = runtime.context_block(goal="pytest memory", tools=["pytest"], max_items=3)

    assert block.items
    assert block.items[0]["source"] == MemorySource.VERIFICATION.value
    assert block.items[0]["last_verified_at"]


def test_cli_memory_commands_cover_candidate_lifecycle(monkeypatch, tmp_path: Path) -> None:
    runtime = MemoryRuntime(tmp_path)
    runtime.start_session(session_id="session_cli", user_goal="cli")
    candidate = MemoryCandidate(
        id="cand_cli",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.USER,
        title="CLI lesson",
        body="Use miniharness memory doctor locally.",
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.USER,
                    ref_id="user_message",
                    summary="manual candidate",
                )
            ]
        ),
    )
    runtime.store.upsert_candidate(candidate)
    monkeypatch.chdir(tmp_path)

    accept = runner.invoke(app, ["memory", "accept", "cand_cli"])
    listed = runner.invoke(app, ["memory", "list"])
    searched = runner.invoke(app, ["memory", "search", "doctor"])
    shown = runner.invoke(app, ["memory", "show", "mem_cand_cli"])
    doctor = runner.invoke(app, ["memory", "doctor"])
    deleted = runner.invoke(app, ["memory", "delete", "mem_cand_cli"])
    rejected = runner.invoke(app, ["memory", "reject", "cand_cli"])
    refresh = runner.invoke(app, ["memory", "refresh"])

    assert accept.exit_code == 0
    assert "accepted" in accept.output
    assert "CLI lesson" in listed.output
    assert "CLI lesson" in searched.output
    assert "Use miniharness memory doctor locally." in shown.output
    assert doctor.exit_code == 0
    assert "ok" in doctor.output
    assert deleted.exit_code == 0
    assert "deleted" in deleted.output
    assert rejected.exit_code == 0
    assert refresh.exit_code == 0


def test_runtime_manual_accept_redacts_and_requires_non_guess_content(tmp_path: Path) -> None:
    runtime = MemoryRuntime(tmp_path)
    runtime.start_session(session_id="session_cli", user_goal="cli")
    runtime.store.upsert_candidate(
        MemoryCandidate(
            id="cand_manual",
            scope=MemoryScope.PROJECT,
            type=MemoryType.LESSON,
            source=MemorySource.MODEL,
            title="Manual lesson",
            body="Use focused pytest with API_KEY=sk-secret for memory tests.",
            provenance=Provenance(),
        )
    )
    runtime.store.upsert_candidate(
        MemoryCandidate(
            id="cand_guess",
            scope=MemoryScope.PROJECT,
            type=MemoryType.LESSON,
            source=MemorySource.MODEL,
            title="Guess",
            body="Model guessed this may be true.",
            provenance=Provenance(),
        )
    )

    entry = runtime.accept_candidate("cand_manual")

    assert entry.status.value == "active"
    assert any(evidence.source == MemorySource.MANUAL for evidence in entry.provenance.evidence)
    assert "sk-secret" not in entry.body
    try:
        runtime.accept_candidate("cand_guess")
    except ValueError as exc:
        assert "quarantined" in str(exc)
    else:
        raise AssertionError("failure guess was accepted into active memory")
