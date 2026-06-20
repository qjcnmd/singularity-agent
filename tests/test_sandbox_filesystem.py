from pathlib import Path

import pytest

from miniharness.sandbox import (
    SandboxFilesystemManager,
    SandboxFilesystemMode,
    SandboxFilesystemPolicy,
)


def test_copy_on_write_copies_regular_files_and_excludes_heavy_dirs(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "app.py").write_text("print('ok')\n", encoding="utf-8")
    (tmp_path / ".env").write_text("OPENAI_API_KEY=sk-test-secret\n", encoding="utf-8")
    (tmp_path / ".env.local").write_text("OPENAI_API_KEY=sk-local-secret\n", encoding="utf-8")
    (tmp_path / ".env.production").write_text("OPENAI_API_KEY=sk-prod-secret\n", encoding="utf-8")
    (tmp_path / "service-token.json").write_text("secret", encoding="utf-8")
    (tmp_path / "certificate.pem").write_text("private", encoding="utf-8")
    (tmp_path / "client.pfx").write_text("private", encoding="utf-8")
    (tmp_path / "signing.p12").write_text("private", encoding="utf-8")
    (tmp_path / "id_rsa").write_text("private", encoding="utf-8")
    for ignored in [".git", "node_modules", "venv", ".pytest_cache"]:
        (tmp_path / ignored).mkdir()
        (tmp_path / ignored / "ignored.txt").write_text("ignored", encoding="utf-8")

    manager = SandboxFilesystemManager()
    prepared = manager.prepare_filesystem(
        sandbox_id="sandbox_fs",
        policy=SandboxFilesystemPolicy(
            mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
            workspace_root=tmp_path,
            sandbox_root=tmp_path / "work" / "sandboxes" / "sandbox_fs",
        ),
        cwd=tmp_path / "src",
    )

    assert (prepared.workspace_copy_root / "src" / "app.py").exists()
    assert not (prepared.workspace_copy_root / ".git").exists()
    assert not (prepared.workspace_copy_root / "node_modules").exists()
    assert not (prepared.workspace_copy_root / ".env").exists()
    assert not (prepared.workspace_copy_root / ".env.local").exists()
    assert not (prepared.workspace_copy_root / ".env.production").exists()
    assert not (prepared.workspace_copy_root / "service-token.json").exists()
    assert not (prepared.workspace_copy_root / "certificate.pem").exists()
    assert not (prepared.workspace_copy_root / "client.pfx").exists()
    assert not (prepared.workspace_copy_root / "signing.p12").exists()
    assert not (prepared.workspace_copy_root / "id_rsa").exists()
    assert prepared.execution_cwd == prepared.workspace_copy_root / "src"


def test_cwd_outside_workspace_fails(tmp_path: Path) -> None:
    outside = tmp_path.parent / f"{tmp_path.name}_outside"
    outside.mkdir()
    manager = SandboxFilesystemManager()

    with pytest.raises(ValueError, match="outside workspace"):
        manager.prepare_filesystem(
            sandbox_id="sandbox_outside",
            policy=SandboxFilesystemPolicy(
                mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
                workspace_root=tmp_path,
                sandbox_root=tmp_path / "work" / "sandboxes" / "sandbox_outside",
            ),
            cwd=outside,
        )


def test_change_detection_tracks_created_modified_deleted_and_cleanup(tmp_path: Path) -> None:
    (tmp_path / "keep.txt").write_text("before", encoding="utf-8")
    (tmp_path / "delete.txt").write_text("remove", encoding="utf-8")
    manager = SandboxFilesystemManager()
    prepared = manager.prepare_filesystem(
        sandbox_id="sandbox_changes",
        policy=SandboxFilesystemPolicy(
            mode=SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
            workspace_root=tmp_path,
            sandbox_root=tmp_path / "work" / "sandboxes" / "sandbox_changes",
        ),
        cwd=tmp_path,
    )
    baseline = manager.capture_baseline(prepared.workspace_copy_root)

    (prepared.workspace_copy_root / "created.txt").write_text("new", encoding="utf-8")
    (prepared.workspace_copy_root / "keep.txt").write_text("after", encoding="utf-8")
    (prepared.workspace_copy_root / "delete.txt").unlink()

    changes = manager.detect_changes(prepared.workspace_copy_root, baseline)
    manager.cleanup(prepared.sandbox_root)

    assert changes.created_files == ["created.txt"]
    assert changes.modified_files == ["keep.txt"]
    assert changes.deleted_files == ["delete.txt"]
    assert changes.total_changed_files == 3
    assert not prepared.sandbox_root.exists()
