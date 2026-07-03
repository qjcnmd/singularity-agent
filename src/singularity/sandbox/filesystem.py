from __future__ import annotations

import fnmatch
import hashlib
import os
import shutil
import stat
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from uuid import uuid4

from singularity.sandbox.models import (
    SandboxChangeSummary,
    SandboxFilesystemMode,
    SandboxFilesystemPolicy,
)


@dataclass(frozen=True)
class FilesystemPrepared:
    sandbox_root: Path
    workspace_copy_root: Path
    execution_cwd: Path
    artifact_root: Path


class SandboxFilesystemManager:
    def __init__(self, *, default_root_name: str = "work/sandboxes") -> None:
        self.default_root_name = default_root_name

    def prepare_filesystem(
        self,
        *,
        sandbox_id: str,
        policy: SandboxFilesystemPolicy,
        cwd: Path,
    ) -> FilesystemPrepared:
        workspace_root = self._resolve(policy.workspace_root)
        raw_cwd = cwd if cwd.is_absolute() else workspace_root / cwd
        resolved_cwd = self._resolve(raw_cwd)
        if not self._is_relative_to(resolved_cwd, workspace_root):
            raise ValueError(f"cwd outside workspace: {cwd}")

        sandbox_root = self._resolve(
            policy.sandbox_root
            or (workspace_root / self.default_root_name / sandbox_id)
        )
        if sandbox_root.exists() and any(sandbox_root.iterdir()):
            shutil.rmtree(sandbox_root)
        sandbox_root.mkdir(parents=True, exist_ok=True)
        artifact_root = sandbox_root / "artifacts"
        artifact_root.mkdir(parents=True, exist_ok=True)

        if policy.mode == SandboxFilesystemMode.EMPTY_TEMP_WORKSPACE or policy.mode == SandboxFilesystemMode.ARTIFACT_OUTPUT_ONLY:
            workspace_copy_root = sandbox_root / "workspace"
            workspace_copy_root.mkdir(parents=True, exist_ok=True)
            execution_cwd = workspace_copy_root
        elif policy.mode in {
            SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE,
            SandboxFilesystemMode.READ_ONLY_WORKSPACE,
        }:
            workspace_copy_root = sandbox_root / "workspace"
            self._copy_workspace(
                source=workspace_root,
                destination=workspace_copy_root,
                policy=policy,
            )
            execution_cwd = workspace_copy_root / resolved_cwd.relative_to(workspace_root)
            if policy.mode == SandboxFilesystemMode.READ_ONLY_WORKSPACE:
                self._make_readonly(workspace_copy_root)
        else:
            workspace_copy_root = sandbox_root / "workspace"
            workspace_copy_root.mkdir(parents=True, exist_ok=True)
            execution_cwd = workspace_copy_root
        execution_cwd.mkdir(parents=True, exist_ok=True)
        return FilesystemPrepared(
            sandbox_root=sandbox_root,
            workspace_copy_root=workspace_copy_root,
            execution_cwd=execution_cwd,
            artifact_root=artifact_root,
        )

    def capture_baseline(self, root: Path) -> dict[str, dict[str, object]]:
        baseline: dict[str, dict[str, object]] = {}
        if not root.exists():
            return baseline
        for path in sorted(root.rglob("*")):
            if not path.is_file() or path.is_symlink():
                continue
            try:
                relative = path.relative_to(root).as_posix()
                stat = path.stat()
                baseline[relative] = {
                    "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                    "size": stat.st_size,
                    "mtime_ns": stat.st_mtime_ns,
                }
            except OSError:
                continue
        return baseline

    def detect_changes(
        self,
        root: Path,
        baseline: dict[str, dict[str, object]],
    ) -> SandboxChangeSummary:
        after = self.capture_baseline(root)
        created = sorted(path for path in after if path not in baseline)
        deleted = sorted(path for path in baseline if path not in after)
        modified = sorted(
            path
            for path, entry in after.items()
            if path in baseline and entry.get("sha256") != baseline[path].get("sha256")
        )
        total = len(created) + len(modified) + len(deleted)
        diff_preview = "\n".join(
            [*(f"A {path}" for path in created[:20]), *(f"M {path}" for path in modified[:20]), *(f"D {path}" for path in deleted[:20])]
        )
        return SandboxChangeSummary(
            created_files=created,
            modified_files=modified,
            deleted_files=deleted,
            total_changed_files=total,
            diff_preview=diff_preview or None,
            importable=False,
        )

    def cleanup(self, sandbox_root: Path) -> None:
        if sandbox_root.exists():
            _clear_readonly_tree(sandbox_root)
            shutil.rmtree(sandbox_root)

    @staticmethod
    def _make_readonly(root: Path) -> None:
        # Clearing the write bits sets the read-only attribute on Windows
        # and removes write permission on POSIX. Failure means the requested
        # read-only capability was not enforced and must fail closed.
        write_bits = stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH
        failures: list[str] = []
        for path in (root, *root.rglob("*")):
            try:
                os.chmod(path, path.stat().st_mode & ~write_bits)
            except OSError:
                failures.append(str(path))
        if failures:
            preview = ", ".join(failures[:5])
            suffix = "" if len(failures) <= 5 else f", +{len(failures) - 5} more"
            raise OSError(f"readonly sandbox capability failed for: {preview}{suffix}")

    def _copy_workspace(
        self,
        *,
        source: Path,
        destination: Path,
        policy: SandboxFilesystemPolicy,
    ) -> None:
        destination.mkdir(parents=True, exist_ok=True)
        for path in sorted(source.rglob("*")):
            try:
                relative = path.relative_to(source)
            except ValueError:
                continue
            rel_posix = relative.as_posix()
            if self._is_excluded(rel_posix, relative.parts, policy.exclude_globs):
                if path.is_dir():
                    continue
                continue
            resolved = self._resolve(path)
            if not self._is_relative_to(resolved, source):
                continue
            target = destination / relative
            if path.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            elif path.is_file() and not path.is_symlink():
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)

    @staticmethod
    def _is_excluded(rel_posix: str, parts: tuple[str, ...], patterns: list[str]) -> bool:
        for pattern in patterns:
            normalized = pattern.replace("\\", "/").strip("/")
            if not normalized:
                continue
            if normalized in parts:
                return True
            if fnmatch.fnmatchcase(rel_posix, normalized) or fnmatch.fnmatchcase(rel_posix, f"{normalized}/**"):
                return True
        return False

    @staticmethod
    def _resolve(path: Path) -> Path:
        return path.expanduser().resolve(strict=False)

    @staticmethod
    def _is_relative_to(child: Path, parent: Path) -> bool:
        try:
            child_key = os.path.normcase(os.path.normpath(str(child)))
            parent_key = os.path.normcase(os.path.normpath(str(parent)))
            return os.path.commonpath([child_key, parent_key]) == parent_key
        except ValueError:
            return False


def random_trace_id() -> str:
    return f"sandbox_trace_{uuid4().hex[:12]}"


def _clear_readonly_tree(root: Path) -> None:
    # Restore write access on the staged tree so shutil.rmtree can remove it
    # even when READ_ONLY_WORKSPACE marked files/directories read-only.
    for path in (root, *root.rglob("*")):
        with suppress(OSError):
            os.chmod(path, path.stat().st_mode | stat.S_IWRITE)
