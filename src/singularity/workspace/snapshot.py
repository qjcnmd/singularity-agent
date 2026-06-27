from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from singularity.workspace.errors import MutationError
from singularity.policy import PermissionProfile
from singularity.workspace.pathing import WorkspacePathResolver


@dataclass(frozen=True)
class FileSnapshot:
    path: str
    sha256: str
    size: int
    mtime: float
    encoding: str | None
    line_ending: Literal["lf", "crlf", "mixed", "none"] | None
    is_binary: bool

    @classmethod
    def from_path(cls, path: Path, *, relative_path: str) -> "FileSnapshot":
        try:
            raw = path.read_bytes()
            stat = path.stat()
        except FileNotFoundError as exc:
            raise MutationError(
                "file_not_found",
                f"File does not exist: {relative_path}",
                {"path": relative_path},
            ) from exc
        is_binary = looks_binary(raw[:4096])
        encoding, line_ending = None, None
        if not is_binary:
            encoding = detect_encoding(raw)
            text = raw.decode(encoding, errors="strict")
            line_ending = detect_line_ending(text)
        return cls(
            path=relative_path,
            sha256=hash_bytes(raw),
            size=stat.st_size,
            mtime=stat.st_mtime,
            encoding=encoding,
            line_ending=line_ending,
            is_binary=is_binary,
        )


class WorkspaceIndex:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        permission_profile: PermissionProfile | None = None,
    ) -> None:
        self.resolver = WorkspacePathResolver(
            workspace_root, permission_profile=permission_profile
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


def hash_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def looks_binary(raw: bytes) -> bool:
    return b"\x00" in raw


def detect_encoding(raw: bytes) -> str:
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise MutationError(
            "encoding_error",
            "File is not valid UTF-8 text.",
            {"error": str(exc)},
        ) from exc
    return "utf-8"


def detect_line_ending(text: str) -> Literal["lf", "crlf", "mixed", "none"]:
    crlf = text.count("\r\n")
    lf = text.count("\n") - crlf
    if crlf and lf:
        return "mixed"
    if crlf:
        return "crlf"
    if lf:
        return "lf"
    return "none"
