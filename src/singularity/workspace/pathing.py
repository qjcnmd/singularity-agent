from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from singularity.policy import PermissionProfile
from singularity.workspace.errors import MutationError


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
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        permission_profile: PermissionProfile | None = None,
        default_access: str | None = None,
    ) -> None:
        self.workspace_root = WorkspaceRoot(workspace_root).path
        self.permission_profile = permission_profile
        self.default_access = default_access
        additional = (
            permission_profile.additional_writable_directories
            if permission_profile is not None
            else ()
        )
        self.authorized_roots = (
            self.workspace_root,
            *(WorkspaceRoot(path).path for path in additional),
        )

    def resolve(
        self,
        user_path: str | Path,
        *,
        access: str | None = None,
    ) -> ResolvedWorkspacePath:
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

        lexical_root = self._containing_root(lexical_candidate)
        resolved_root = self._containing_root(resolved)
        if resolved_root is None or (
            lexical_root is not None and lexical_root != resolved_root
        ):
            if lexical_root is not None and self._contains_symlink_component(
                lexical_candidate, lexical_root
            ):
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

        effective_access = access or self.default_access
        if self.permission_profile is not None and effective_access is not None:
            rule = self.permission_profile.matching_protected_rule(
                resolved, access=effective_access
            )
            if rule is not None and rule.hard_deny:
                raise MutationError(
                    "protected_path_denied",
                    "Protected path access is denied.",
                    {"path": resolved.name},
                )

        relative = (
            Path(os.path.relpath(str(resolved), str(self.workspace_root)))
            if resolved_root == self.workspace_root
            else resolved
        )
        return ResolvedWorkspacePath(
            input_path=str(user_path),
            path=resolved,
            relative_path=relative,
            workspace_root=resolved_root,
        )

    def _containing_root(self, candidate: Path) -> Path | None:
        for root in self.authorized_roots:
            if self._is_inside(candidate, root):
                return root
        return None

    def _is_inside(self, candidate: Path, root: Path) -> bool:
        try:
            root_key = self._path_key(root)
            candidate_key = self._path_key(candidate)
            return os.path.commonpath([root_key, candidate_key]) == root_key
        except (OSError, ValueError):
            return False

    def _contains_symlink_component(
        self, lexical_candidate: Path, root: Path
    ) -> bool:
        try:
            relative = Path(os.path.relpath(str(lexical_candidate), str(root)))
        except ValueError:
            return False

        current = root
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
