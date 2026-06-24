from __future__ import annotations

import hashlib
import os
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Iterable

from singularity.code_index.exceptions import IndexBudgetExceededError, PathOutsideWorkspaceError
from singularity.code_index.models import (
    BackendInfo,
    Evidence,
    FileRecord,
    FileRole,
    LanguageId,
    TrustLevel,
)


DEFAULT_IGNORE_DIRS = {
    ".git",
    ".singularity",
    ".mypy_cache",
    ".next",
    ".pytest_cache",
    ".ruff_cache",
    ".turbo",
    ".venv",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "venv",
}
CONFIG_NAMES = {
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.json",
    ".pre-commit-config.yaml",
    ".ruff.toml",
    "Cargo.toml",
    "Dockerfile",
    "Makefile",
    "eslint.config.js",
    "justfile",
    "mypy.ini",
    "package.json",
    "pnpm-workspace.yaml",
    "pyproject.toml",
    "pytest.ini",
    "requirements.txt",
    "ruff.toml",
    "setup.cfg",
    "setup.py",
    "tox.ini",
    "tsconfig.json",
}
LOCK_NAMES = {
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "poetry.lock",
    "uv.lock",
    "yarn.lock",
}
DOC_SUFFIXES = {".adoc", ".md", ".mdx", ".rst", ".txt"}
SOURCE_SUFFIXES = {
    ".go",
    ".java",
    ".js",
    ".jsx",
    ".py",
    ".rs",
    ".ts",
    ".tsx",
}


@dataclass(frozen=True)
class ScannerBudget:
    max_files: int = 20_000
    max_file_size: int = 1_000_000
    max_total_bytes: int = 50_000_000


class WorkspaceScanner:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        ignore_dirs: set[str] | None = None,
        budget: ScannerBudget | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.ignore_dirs = set(ignore_dirs or DEFAULT_IGNORE_DIRS)
        self.budget = budget or ScannerBudget()
        self.backend = BackendInfo(name="workspace_scanner", version="1.0.0")

    def scan(self) -> list[FileRecord]:
        records: list[FileRecord] = []
        total_bytes = 0
        for dirpath, dirnames, filenames in os.walk(self.workspace_root):
            current = Path(dirpath)
            dirnames[:] = [
                name
                for name in dirnames
                if name not in self.ignore_dirs and not self._is_build_dir(current / name)
            ]
            for filename in sorted(filenames):
                path = current / filename
                if self._ignored(path):
                    continue
                record = self._record_for(path)
                records.append(record)
                total_bytes += min(record.size_bytes, self.budget.max_file_size)
                if len(records) > self.budget.max_files:
                    raise IndexBudgetExceededError("max_files", self.budget.max_files)
                if total_bytes > self.budget.max_total_bytes:
                    raise IndexBudgetExceededError("max_total_bytes", self.budget.max_total_bytes)
        return records

    def scan_paths(self, paths: Iterable[str]) -> list[FileRecord]:
        records: list[FileRecord] = []
        for path in sorted({str(item).replace("\\", "/") for item in paths}):
            resolved = self.resolve(path)
            if not resolved.exists() or not resolved.is_file() or self._ignored(resolved):
                continue
            records.append(self._record_for(resolved))
        return records

    def resolve(self, path: str | Path) -> Path:
        raw = Path(path)
        candidate = raw if raw.is_absolute() else self.workspace_root / raw
        resolved = candidate.resolve(strict=False)
        if not _is_inside(resolved, self.workspace_root):
            raise PathOutsideWorkspaceError(str(path))
        return resolved

    def _record_for(self, path: Path) -> FileRecord:
        resolved = self.resolve(path)
        stat = resolved.stat()
        relative = resolved.relative_to(self.workspace_root).as_posix()
        is_binary = _looks_binary(resolved)
        roles = self._roles(relative, is_binary=is_binary)
        return FileRecord(
            path=relative,
            language=self._language(relative),
            roles=roles,
            size_bytes=stat.st_size,
            sha256=_sha256(resolved) if stat.st_size <= self.budget.max_file_size else None,
            mtime_ns=stat.st_mtime_ns,
            is_binary=is_binary,
            is_hidden=any(part.startswith(".") for part in PurePosixPath(relative).parts),
            line_count=None if is_binary or stat.st_size > self.budget.max_file_size else _line_count(resolved),
            confidence=1.0,
            evidence=[
                Evidence(
                    source="filesystem",
                    path=relative,
                    description="WorkspaceScanner stat/hash classification.",
                )
            ],
            trust_level=TrustLevel.COMPONENT_GENERATED,
            backend=self.backend,
            source="workspace_scanner",
        )

    def _roles(self, relative: str, *, is_binary: bool) -> list[FileRole]:
        pure = PurePosixPath(relative)
        name = pure.name
        lower_name = name.lower()
        parts = {part.lower() for part in pure.parts}
        roles: set[FileRole] = set()
        if is_binary:
            roles.add(FileRole.BINARY)
        if any(part.startswith(".") for part in pure.parts):
            roles.add(FileRole.HIDDEN)
        if name in CONFIG_NAMES or lower_name in {item.lower() for item in CONFIG_NAMES}:
            roles.add(FileRole.CONFIG)
        if name in LOCK_NAMES:
            roles.add(FileRole.LOCKFILE)
            roles.add(FileRole.CONFIG)
        if pure.suffix.lower() in DOC_SUFFIXES or "docs" in parts:
            roles.add(FileRole.DOC)
        if pure.suffix.lower() in SOURCE_SUFFIXES:
            roles.add(FileRole.SOURCE)
        if _is_test_path(pure):
            roles.add(FileRole.TEST)
        if _is_entrypoint_path(pure):
            roles.add(FileRole.ENTRYPOINT)
        if "generated" in parts or lower_name.endswith((".generated.py", ".generated.ts", ".g.py")):
            roles.add(FileRole.GENERATED)
        if "vendor" in parts or "third_party" in parts:
            roles.add(FileRole.VENDOR)
        if self._is_build_dir(self.workspace_root / relative) or "dist" in parts or "build" in parts:
            roles.add(FileRole.BUILD_ARTIFACT)
        return sorted(roles or {FileRole.UNKNOWN}, key=lambda role: role.value)

    @staticmethod
    def _language(relative: str) -> LanguageId:
        pure = PurePosixPath(relative)
        suffix = pure.suffix.lower()
        if suffix == ".py":
            return LanguageId.PYTHON
        if suffix in {".js", ".jsx", ".mjs", ".cjs"}:
            return LanguageId.JAVASCRIPT
        if suffix in {".ts", ".tsx"}:
            return LanguageId.TYPESCRIPT
        if suffix == ".rs":
            return LanguageId.RUST
        if suffix in {".md", ".mdx", ".rst", ".adoc"}:
            return LanguageId.MARKDOWN
        if suffix == ".json":
            return LanguageId.JSON
        if suffix == ".toml":
            return LanguageId.TOML
        if suffix in {".yaml", ".yml"}:
            return LanguageId.YAML
        if suffix == ".txt":
            return LanguageId.TEXT
        return LanguageId.UNKNOWN

    def _ignored(self, path: Path) -> bool:
        try:
            relative = path.relative_to(self.workspace_root)
        except ValueError:
            return True
        return any(part in self.ignore_dirs for part in relative.parts)

    @staticmethod
    def _is_build_dir(path: Path) -> bool:
        return path.name in {"dist", "build", "coverage", "target", ".next", ".turbo"}


def _is_inside(path: Path, root: Path) -> bool:
    try:
        root_key = os.path.normcase(os.path.normpath(str(root.resolve(strict=False))))
        path_key = os.path.normcase(os.path.normpath(str(path.resolve(strict=False))))
        return os.path.commonpath([root_key, path_key]) == root_key
    except (OSError, ValueError):
        return False


def _looks_binary(path: Path) -> bool:
    try:
        chunk = path.read_bytes()[:4096]
    except OSError:
        return True
    if b"\0" in chunk:
        return True
    if not chunk:
        return False
    text_like = sum(byte in b"\t\r\n\f\b" or 32 <= byte <= 126 for byte in chunk)
    return text_like / max(1, len(chunk)) < 0.70


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _line_count(path: Path) -> int:
    try:
        with path.open("r", encoding="utf-8", errors="ignore") as file:
            return sum(1 for _ in file)
    except OSError:
        return 0


def _is_test_path(path: PurePosixPath) -> bool:
    name = path.name.lower()
    parts = {part.lower() for part in path.parts}
    return (
        "tests" in parts
        or name.startswith("test_")
        or name.endswith("_test.py")
        or ".test." in name
        or ".spec." in name
    )


def _is_entrypoint_path(path: PurePosixPath) -> bool:
    normalized = path.as_posix()
    return normalized in {
        "main.py",
        "app.py",
        "src/main.rs",
        "src/lib.rs",
        "src/main.ts",
        "src/index.ts",
        "src/main.js",
        "src/index.js",
    } or path.name in {"cli.py", "__main__.py"}
