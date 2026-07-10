from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from singularity.memory.models import (
    Confidence,
    MemoryEntry,
    MemoryScope,
    MemorySource,
    MemoryType,
)
from singularity.memory.pipeline import MemoryLearningPipeline
from singularity.memory.sync import MemoryBundleSync
from singularity.oracle.cli import app

runner = CliRunner()


def _entry() -> MemoryEntry:
    return MemoryEntry(
        id="mem_remote_fact",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.MANUAL,
        title="Remote sync fact",
        body="Remote memory sync imports active entries as reviewable candidates by default.",
        confidence=Confidence.HIGH,
        paths=["README.md"],
    )


def test_memory_sync_exports_bundle_and_imports_entries_as_candidates(tmp_path: Path) -> None:
    source = MemoryLearningPipeline(tmp_path / "source")
    source.start_session(session_id="source", user_goal="sync")
    source.store.upsert_entry(_entry())
    bundle_path = tmp_path / "bundle.json"

    exported = MemoryBundleSync(source.store).export_bundle(bundle_path)
    target = MemoryLearningPipeline(tmp_path / "target")
    target.start_session(session_id="target", user_goal="sync")
    imported = MemoryBundleSync(target.store).import_bundle(bundle_path)

    assert exported.path == bundle_path
    payload = json.loads(bundle_path.read_text(encoding="utf-8"))
    assert payload["schema_version"] == "singularity.memory_sync_bundle/v1"
    assert payload["content_digest"]
    assert imported.entries_as_candidates == 1
    assert target.store.load_entries() == []
    candidates = target.store.load_candidates()
    assert candidates[0].metadata["remote_source_entry_id"] == "mem_remote_fact"


def test_memory_sync_cli_export_and_import(monkeypatch, tmp_path: Path) -> None:
    source_root = tmp_path / "source"
    target_root = tmp_path / "target"
    target_root.mkdir()
    source = MemoryLearningPipeline(source_root)
    source.start_session(session_id="source", user_goal="sync")
    source.store.upsert_entry(_entry())
    bundle_path = tmp_path / "memory-bundle.json"

    monkeypatch.chdir(source_root)
    exported = runner.invoke(app, ["memory", "sync", "export", str(bundle_path), "--json"])

    monkeypatch.chdir(target_root)
    imported = runner.invoke(app, ["memory", "sync", "import", str(bundle_path), "--json"])

    assert exported.exit_code == 0, exported.output
    assert imported.exit_code == 0, imported.output
    assert json.loads(exported.output)["entries"] == 1
    assert json.loads(imported.output)["entries_as_candidates"] == 1
