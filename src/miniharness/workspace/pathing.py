from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from miniharness.workspace.errors import MutationError


@dataclass(frozen=True)
class ResolvedWorkspacePath:
    input_path: str
    path: Path
    relative_path: Path
    workspace_root: Path

    @property
    def relative_posix(self) -> str:
        return "." if str(self.relative_path) == "." else self.relative_path.as_posix()


class WorkspaceRoot:
    def __init__(self, root: Path | str) -> None:
        self.path = Path(root).expanduser().resolve(strict=False)
        if not self.path.exists():
            raise MutationError(
                "path_outside_workspace",
                f"Workspace root does not exist: {self.path}",
            )
        if not self.path.is_dir():
            raise MutationError(
                "path_outside_workspace",
                f"Workspace root is not a directory: {self.path}",
            )


class WorkspacePathResolver:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = WorkspaceRoot(workspace_root).path

    def resolve(self, user_path: str | Path) -> ResolvedWorkspacePath:
        raw = Path(user_path)
        candidate = raw if raw.is_absolute() else self.workspace_root / raw
        lexical_candidate = candidate.absolute()
        try:
            resolved = candidate.resolve(strict=False)
        except OSError as exc:
            raise MutationError(
                "path_outside_workspace",
                f"Could not resolve path: {user_path}",
                {"error": str(exc)},
            ) from exc

        lexical_inside = self._is_inside(lexical_candidate)
        resolved_inside = self._is_inside(resolved)
        if not resolved_inside:
            if lexical_inside and self._contains_symlink_component(lexical_candidate):
                raise MutationError(
                    "symlink_escape",
                    f"Path resolves through a symlink outside the workspace: {user_path}",
                    {"path": str(resolved)},
                )
            raise MutationError(
                "path_outside_workspace",
                f"Path is outside the workspace: {user_path}",
                {"path": str(resolved)},
            )

        relative = Path(os.path.relpath(str(resolved), str(self.workspace_root)))
        return ResolvedWorkspacePath(
            input_path=str(user_path),
            path=resolved,
            relative_path=relative,
            workspace_root=self.workspace_root,
        )

    def _is_inside(self, candidate: Path) -> bool:
        try:
            root_key = self._path_key(self.workspace_root)
            candidate_key = self._path_key(candidate)
            return os.path.commonpath([root_key, candidate_key]) == root_key
        except (OSError, ValueError):
            return False

    def _contains_symlink_component(self, lexical_candidate: Path) -> bool:
        try:
            relative = Path(os.path.relpath(str(lexical_candidate), str(self.workspace_root)))
        except ValueError:
            return False

        current = self.workspace_root
        for part in relative.parts:
            if part in {"", "."}:
                continue
            current = current / part
            if current.is_symlink():
                return True
            if not current.exists():
                return False
        return False

    @staticmethod
    def _path_key(path: Path) -> str:
        return os.path.normcase(os.path.normpath(str(path)))
