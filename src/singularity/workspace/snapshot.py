from __future__ import annotations

from pathlib import Path

from singularity.policy import PermissionProfile
from singularity.workspace.errors import MutationError
from singularity.workspace_core import (
    WorkspaceFileSnapshot,
    WorkspacePathResolver,
    detect_encoding,
    detect_line_ending,
    hash_bytes,
    looks_binary,
)


class FileSnapshot(WorkspaceFileSnapshot):
    pass


class WorkspaceIndex:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        permission_profile: PermissionProfile | None = None,
    ) -> None:
        self.resolver = WorkspacePathResolver(
            workspace_root,
            permission_profile=permission_profile,
        )
        self.snapshots: dict[str, FileSnapshot] = {}

    def snapshot_file(self, user_path: str | Path) -> FileSnapshot:
        resolved = self.resolver.resolve(user_path)
        if not resolved.path.exists():
            raise MutationError(
                "file_not_found",
                f"File does not exist: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        if not resolved.path.is_file():
            raise MutationError(
                "invalid_operation",
                f"Path is not a file: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        snapshot = FileSnapshot.from_path(
            resolved.path,
            relative_path=resolved.relative_posix,
        )
        self.snapshots[snapshot.path] = snapshot
        return snapshot

    def snapshot_optional(self, user_path: str | Path) -> FileSnapshot | None:
        resolved = self.resolver.resolve(user_path)
        if not resolved.path.exists():
            return None
        return self.snapshot_file(user_path)

    def current_hash(self, user_path: str | Path) -> str | None:
        resolved = self.resolver.resolve(user_path)
        if not resolved.path.exists():
            return None
        return hash_bytes(resolved.path.read_bytes())


__all__ = [
    "FileSnapshot",
    "WorkspaceIndex",
    "detect_encoding",
    "detect_line_ending",
    "hash_bytes",
    "looks_binary",
]
