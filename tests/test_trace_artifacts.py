from __future__ import annotations

import hashlib
from pathlib import Path

import pytest

from miniharness.observability.artifacts import TraceArtifactStore
from miniharness.observability.exceptions import TraceArtifactError
from miniharness.observability.models import TraceArtifactKind


def test_text_and_bytes_artifacts_are_written_with_sha256(tmp_path: Path) -> None:
    store = TraceArtifactStore(tmp_path, run_id="run_1", session_id="session_1")

    text_artifact = store.write_text_artifact(
        kind=TraceArtifactKind.STDOUT,
        text="hello OPENAI_API_KEY=sk-secret",
        task_id="task_1",
        summary="stdout",
    )
    bytes_artifact = store.write_bytes_artifact(
        kind=TraceArtifactKind.GENERIC,
        data=b"\x00\x01",
        task_id="task_1",
        content_type="application/octet-stream",
    )

    assert text_artifact.path.exists()
    assert bytes_artifact.path.exists()
    assert text_artifact.sha256 == hashlib.sha256(text_artifact.path.read_bytes()).hexdigest()
    assert bytes_artifact.sha256 == hashlib.sha256(b"\x00\x01").hexdigest()
    assert "sk-secret" not in text_artifact.path.read_text(encoding="utf-8")
    assert text_artifact.relative_path.startswith("artifacts/")


def test_artifact_limits_and_metadata_redaction(tmp_path: Path) -> None:
    store = TraceArtifactStore(
        tmp_path,
        run_id="run_1",
        session_id="session_1",
        max_artifact_bytes=8,
        max_total_bytes=100,
    )

    artifact = store.write_text_artifact(
        kind=TraceArtifactKind.REPORT,
        text="ok",
        metadata={"token": "secret-token", "label": "safe"},
    )

    assert artifact.metadata["token"] == "<redacted>"
    assert artifact.metadata["label"] == "safe"
    with pytest.raises(TraceArtifactError):
        store.write_text_artifact(kind=TraceArtifactKind.STDOUT, text="x" * 100)


def test_register_file_artifact_copies_and_events_only_need_artifact_id(tmp_path: Path) -> None:
    source = tmp_path / "source.log"
    source.write_text("large output", encoding="utf-8")
    store = TraceArtifactStore(tmp_path, run_id="run_1", session_id="session_1")

    artifact = store.register_file_artifact(
        kind=TraceArtifactKind.COMMAND_LOG,
        source_path=source,
        summary="command log",
    )

    assert artifact.artifact_id.startswith("artifact_")
    assert artifact.path.read_text(encoding="utf-8") == "large output"
    event_payload = {"artifact_refs": [artifact.artifact_id]}
    assert event_payload == {"artifact_refs": [artifact.artifact_id]}
    assert "large output" not in str(event_payload)


def test_artifact_store_resolves_by_opaque_artifact_id(tmp_path: Path) -> None:
    store = TraceArtifactStore(tmp_path, run_id="run_1", session_id="session_1")
    artifact = store.write_text_artifact(
        kind=TraceArtifactKind.REPORT,
        text="hello",
        summary="report",
    )

    payload = artifact.to_dict()

    assert payload["artifact_ref"] == artifact.artifact_id
    assert "path" not in payload
    assert store.read_artifact(artifact.artifact_id) == b"hello"
