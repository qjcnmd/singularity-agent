import hashlib
from pathlib import Path

from singularity.sandbox import (
    SandboxArtifactCollector,
    SandboxResourceLimits,
)


def test_artifact_file_content_is_redacted(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    artifact_root = tmp_path / "artifacts"
    secret_text = "token=sk-test123 and aws=AKIAIOSFODNN7EXAMPLE\n"
    (workspace / "report.txt").write_text(secret_text, encoding="utf-8")

    collector = SandboxArtifactCollector()
    artifacts = collector.collect(
        sandbox_id="sandbox_redact",
        workspace_root=workspace,
        artifact_root=artifact_root,
        artifact_paths=["report.txt"],
        limits=SandboxResourceLimits(max_artifact_bytes=1024 * 1024),
    )

    file_artifacts = [item for item in artifacts if item.relative_path == "report.txt"]
    assert len(file_artifacts) == 1
    artifact = file_artifacts[0]
    assert artifact.redacted is True

    stored_bytes = artifact.path.read_bytes()
    stored = stored_bytes.decode("utf-8")
    assert "sk-test123" not in stored
    assert "AKIAIOSFODNN7EXAMPLE" not in stored
    assert "<redacted:" in stored

    expected_sha = hashlib.sha256(stored_bytes).hexdigest()
    assert artifact.sha256 == expected_sha
    assert artifact.size_bytes == len(stored_bytes)


def test_artifact_binary_file_not_redacted(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    artifact_root = tmp_path / "artifacts"
    binary_payload = bytes(range(256)) + b"\xff\xfe\x00\x80"
    (workspace / "blob.bin").write_bytes(binary_payload)

    collector = SandboxArtifactCollector()
    artifacts = collector.collect(
        sandbox_id="sandbox_binary",
        workspace_root=workspace,
        artifact_root=artifact_root,
        artifact_paths=["blob.bin"],
        limits=SandboxResourceLimits(max_artifact_bytes=1024 * 1024),
    )

    file_artifacts = [item for item in artifacts if item.relative_path == "blob.bin"]
    assert len(file_artifacts) == 1
    artifact = file_artifacts[0]
    assert artifact.redacted is False

    stored = artifact.path.read_bytes()
    assert stored == binary_payload

    expected_sha = hashlib.sha256(binary_payload).hexdigest()
    assert artifact.sha256 == expected_sha
    assert artifact.size_bytes == len(binary_payload)


def test_artifact_text_sha256_differs_from_raw(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    artifact_root = tmp_path / "artifacts"
    secret_text = "key=sk-test123\n"
    (workspace / "leak.log").write_text(secret_text, encoding="utf-8")

    collector = SandboxArtifactCollector()
    artifacts = collector.collect(
        sandbox_id="sandbox_sha",
        workspace_root=workspace,
        artifact_root=artifact_root,
        artifact_paths=["leak.log"],
        limits=SandboxResourceLimits(max_artifact_bytes=1024 * 1024),
    )

    artifact = next(item for item in artifacts if item.relative_path == "leak.log")
    raw_sha = hashlib.sha256(secret_text.encode("utf-8")).hexdigest()
    assert artifact.sha256 != raw_sha
    assert "sk-test123" not in artifact.path.read_text(encoding="utf-8")
