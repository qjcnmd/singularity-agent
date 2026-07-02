from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from singularity.cli import app
from singularity.memory.models import (
    MemoryCandidate,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryType,
    Provenance,
)
from singularity.memory.pipeline import MemoryLearningPipeline
from singularity.memory.store import MemoryStore

runner = CliRunner()


def test_memory_pipeline_start_session_ingests_candidates_and_retrieves_context(tmp_path: Path) -> None:
    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_1", user_goal="memory component")
    component.ingest_verification_result(
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

    assert component.store.load_entries() == []
    stored_candidate = component.store.load_candidates()[0]
    accepted = component.accept_candidate(stored_candidate.id)

    block = component.context_block(goal="pytest memory", tools=["pytest"], max_items=3)

    assert block.items
    assert block.items[0]["source"] == MemorySource.VERIFICATION.value
    assert block.items[0]["last_verified_at"]
    assert block.items[0]["id"] == accepted.id


def test_memory_pipeline_loads_human_memory_and_path_scoped_rules(tmp_path: Path) -> None:
    memory_root = tmp_path / ".singularity" / "memory" / "human"
    rules_root = tmp_path / ".singularity" / "rules"
    memory_root.mkdir(parents=True)
    rules_root.mkdir(parents=True)
    memory_root.joinpath("commands.md").write_text(
        "# Commands\n\nUse `python -m pytest tests --basetemp work/pytest-tmp` for verification.\n",
        encoding="utf-8",
    )
    rules_root.joinpath("memory-tests.md").write_text(
        "---\npaths:\n  - tests/memory/**\n---\n# Memory Tests\n\nKeep memory tests focused on JSONL and context injection.\n",
        encoding="utf-8",
    )
    rules_root.joinpath("global.md").write_text(
        "# Global Rule\n\nNever treat memory as approval policy.\n",
        encoding="utf-8",
    )

    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_rules", user_goal="fix memory tests")

    matched = component.context_block(
        goal="pytest memory policy",
        paths=["tests/memory/test_store.py"],
        max_items=5,
        token_budget=120,
    )
    unmatched = component.context_block(
        goal="pytest memory policy",
        paths=["src/singularity/agent.py"],
        max_items=5,
        token_budget=120,
    )

    matched_titles = {item["title"] for item in matched.items}
    unmatched_titles = {item["title"] for item in unmatched.items}
    assert "Commands" in matched_titles
    assert "Memory Tests" in matched_titles
    assert "Global Rule" in matched_titles
    assert "Memory Tests" not in unmatched_titles
    assert "Global Rule" in unmatched_titles


def test_memory_pipeline_does_not_inject_pristine_human_templates(tmp_path: Path) -> None:
    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_empty", user_goal="inspect project")

    block = component.context_block(goal="project preferences lessons commands", max_items=10)

    assert block.items == []


def test_memory_retrieval_records_duration_without_query_content(tmp_path: Path) -> None:
    events: list[tuple[str, dict[str, object]]] = []

    class Trace:
        def record(self, event: str, payload: dict[str, object]) -> None:
            events.append((event, payload))

    component = MemoryLearningPipeline(tmp_path, trace=Trace())
    component.start_session(session_id="session_timing", user_goal="timing")

    component.retrieve(goal="secret query text")

    event, payload = next(item for item in events if item[0] == "retrieval.query.completed")
    assert event == "retrieval.query.completed"
    assert payload["duration_ms"] >= 0
    assert "secret query text" not in json.dumps(payload)


def test_cli_memory_commands_cover_candidate_lifecycle(monkeypatch, tmp_path: Path) -> None:
    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_cli", user_goal="cli")
    candidate = MemoryCandidate(
        id="cand_cli",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.USER,
        title="CLI lesson",
        body="Use singularity memory doctor locally.",
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
    component.store.upsert_candidate(candidate)
    monkeypatch.chdir(tmp_path)

    accept = runner.invoke(app, ["memory", "accept", "cand_cli"])
    listed = runner.invoke(app, ["memory", "list"])
    searched = runner.invoke(app, ["memory", "search", "doctor"])
    shown = runner.invoke(app, ["memory", "show", "mem_cand_cli"])
    doctor = runner.invoke(app, ["memory", "doctor"])
    deleted = runner.invoke(app, ["memory", "delete", "mem_cand_cli"])
    rejected = runner.invoke(app, ["memory", "reject", "cand_cli"])
    refresh = runner.invoke(app, ["memory", "refresh"])
    candidates = runner.invoke(app, ["memory", "candidates"])
    rules = runner.invoke(app, ["memory", "rules", "list"])

    assert accept.exit_code == 0
    assert "accepted" in accept.output
    assert "CLI lesson" in listed.output
    assert "CLI lesson" in searched.output
    assert "Use singularity memory doctor locally." in shown.output
    assert doctor.exit_code == 0
    assert "ok" in doctor.output
    assert deleted.exit_code == 0
    assert "deleted" in deleted.output
    assert rejected.exit_code == 0
    assert refresh.exit_code == 0
    assert candidates.exit_code == 0
    assert rules.exit_code == 0


def test_cli_read_only_memory_commands_do_not_rebuild_index(
    monkeypatch,
    tmp_path: Path,
) -> None:
    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_cli", user_goal="cli")
    component.store.upsert_candidate(
        MemoryCandidate(
            id="cand_read",
            scope=MemoryScope.PROJECT,
            type=MemoryType.LESSON,
            source=MemorySource.USER,
            title="Read-only lesson",
            body="Use singularity memory list for local inspection.",
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
    )
    component.accept_candidate("cand_read")

    def fail_rebuild(_store: MemoryStore) -> dict:
        raise AssertionError("read-only memory command rebuilt the index")

    monkeypatch.setattr(MemoryStore, "rebuild_index", fail_rebuild)
    monkeypatch.chdir(tmp_path)

    for command in (
        ["memory", "list"],
        ["memory", "candidates"],
        ["memory", "show", "mem_cand_read"],
        ["memory", "search", "inspection"],
        ["memory", "doctor"],
        ["memory", "rules", "list"],
    ):
        result = runner.invoke(app, command)
        assert result.exit_code == 0, result.output

    listed_json = runner.invoke(app, ["memory", "list", "--json"])

    assert listed_json.exit_code == 0
    assert json.loads(listed_json.output)[0]["title"] == "Read-only lesson"


def test_memory_pipeline_manual_accept_redacts_and_requires_non_guess_content(tmp_path: Path) -> None:
    component = MemoryLearningPipeline(tmp_path)
    component.start_session(session_id="session_cli", user_goal="cli")
    component.store.upsert_candidate(
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
    component.store.upsert_candidate(
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

    entry = component.accept_candidate("cand_manual")

    assert entry.status.value == "active"
    assert any(evidence.source == MemorySource.MANUAL for evidence in entry.provenance.evidence)
    assert "sk-secret" not in entry.body
    try:
        component.accept_candidate("cand_guess")
    except ValueError as exc:
        assert "quarantined" in str(exc)
    else:
        raise AssertionError("failure guess was accepted into active memory")
