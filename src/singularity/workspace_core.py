from __future__ import annotations

import hashlib
import os
from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar, Literal

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
                lexical_candidate,
                lexical_root,
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
                resolved,
                access=effective_access,
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
        self,
        lexical_candidate: Path,
        root: Path,
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


PUBLIC_SOURCE = "PUBLIC_SOURCE"
PROJECT_CONFIG = "PROJECT_CONFIG"
TEST = "TEST"
DOCUMENTATION = "DOCUMENTATION"
BUILD_SCRIPT = "BUILD_SCRIPT"
DEPENDENCY_LOCK = "DEPENDENCY_LOCK"
SECRET = "SECRET"
VCS_INTERNAL = "VCS_INTERNAL"
GENERATED = "GENERATED"
BINARY = "BINARY"
LARGE_ARTIFACT = "LARGE_ARTIFACT"
UNKNOWN = "UNKNOWN"


class FileClassifier:
    binary_extensions: ClassVar[set[str]] = {
        ".7z",
        ".avif",
        ".bin",
        ".bmp",
        ".class",
        ".dll",
        ".exe",
        ".gif",
        ".ico",
        ".jar",
        ".jpeg",
        ".jpg",
        ".pdf",
        ".png",
        ".pyc",
        ".so",
        ".webp",
        ".zip",
    }
    source_extensions: ClassVar[set[str]] = {
        ".c",
        ".cpp",
        ".cs",
        ".css",
        ".go",
        ".html",
        ".java",
        ".js",
        ".jsx",
        ".json",
        ".mdx",
        ".py",
        ".rs",
        ".sh",
        ".sql",
        ".ts",
        ".tsx",
    }
    docs_extensions: ClassVar[set[str]] = {".adoc", ".md", ".rst", ".txt"}
    config_names: ClassVar[set[str]] = {
        ".editorconfig",
        ".flake8",
        ".gitignore",
        ".pre-commit-config.yaml",
        "hatch.toml",
        "mypy.ini",
        "pyproject.toml",
        "pytest.ini",
        "ruff.toml",
        "setup.cfg",
        "setup.py",
        "tox.ini",
        "tsconfig.json",
    }
    lock_names: ClassVar[set[str]] = {
        "cargo.lock",
        "go.sum",
        "package-lock.json",
        "pnpm-lock.yaml",
        "poetry.lock",
        "requirements.txt",
        "uv.lock",
        "yarn.lock",
    }
    build_names: ClassVar[set[str]] = {"dockerfile", "makefile", "justfile"}
    generated_dirs: ClassVar[set[str]] = {
        ".coverage",
        ".deepeval",
        ".singularity",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        "__pycache__",
        "build",
        "coverage",
        "dist",
        "outputs",
    }

    def __init__(self, *, large_file_bytes: int = 1_000_000) -> None:
        self.large_file_bytes = large_file_bytes

    def classify(
        self,
        resolved: ResolvedWorkspacePath,
        *,
        size: int | None = None,
        is_binary: bool | None = None,
    ) -> str:
        path = resolved.relative_path
        parts = tuple(part.lower() for part in path.parts)
        name = path.name
        lower_name = name.lower()
        suffix = path.suffix.lower()

        if ".git" in parts:
            return VCS_INTERNAL
        if self._is_secret_name(name):
            return SECRET
        if is_binary or suffix in self.binary_extensions:
            return BINARY
        if size is not None and size > self.large_file_bytes:
            return LARGE_ARTIFACT
        if any(part in self.generated_dirs for part in parts):
            return GENERATED
        if lower_name in self.lock_names:
            return DEPENDENCY_LOCK
        if lower_name in self.build_names or suffix in {".ps1", ".bat", ".cmd"}:
            return BUILD_SCRIPT
        if lower_name in self.config_names or suffix in {".ini", ".cfg", ".toml", ".yaml", ".yml"}:
            return PROJECT_CONFIG
        if "tests" in parts or "test" in parts or lower_name.startswith("test_"):
            return TEST
        if "docs" in parts or suffix in self.docs_extensions:
            return DOCUMENTATION
        if suffix in self.source_extensions:
            return PUBLIC_SOURCE
        return UNKNOWN

    @staticmethod
    def _is_secret_name(name: str) -> bool:
        lower = name.lower()
        if lower == ".env":
            return True
        if lower.startswith(".env.") and lower != ".env.example":
            return True
        return lower.endswith((".pem", ".key", ".p12", ".pfx"))


@dataclass(frozen=True)
class WorkspaceFileSnapshot:
    path: str
    sha256: str
    size: int
    mtime: float
    encoding: str | None
    line_ending: Literal["lf", "crlf", "mixed", "none"] | None
    is_binary: bool

    @classmethod
    def from_path(cls, path: Path, *, relative_path: str) -> WorkspaceFileSnapshot:
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
