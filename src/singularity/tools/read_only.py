from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import (
    PermissionLevel,
    ToolCachePolicy,
    ToolIdempotencyPolicy,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolExecutionFailure,
    ToolSpec,
)
from singularity.tools.safety import FileSensitivityClassifier, redact_secret_text


SKIP_DIRS = {
    ".git",
    ".singularity",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
}

# Default per-file size limit for search_text scans. Files larger than this
# are skipped (with a warning entry in the result) to bound scan cost.
DEFAULT_SEARCH_MAX_FILE_BYTES = 10 * 1024 * 1024


class ListFilesInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str = Field(".", description="Directory to list, relative to project root.")
    max_depth: int = Field(4, ge=0, le=10, description="Maximum directory depth.")


class ReadFileInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    path: str = Field(..., description="File path, relative to project root.")
    max_bytes: int = Field(
        20000, ge=1, le=200000, description="Maximum bytes to read."
    )


class SearchTextInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    query: str = Field(..., min_length=1, description="Text to search for.")
    path: str = Field(".", description="File or directory path to search.")
    case_sensitive: bool = Field(False, description="Whether matching is case-sensitive.")
    max_results: int = Field(50, ge=1, le=200, description="Maximum matches to return.")
    max_file_bytes: int = Field(
        10_485_760,
        ge=1,
        description="Skip files larger than this many bytes when scanning.",
    )


class ReadOnlyToolHandlers:
    def __init__(self, project_root: Path) -> None:
        self.project_root = project_root.resolve()
        self.sensitivity = FileSensitivityClassifier(self.project_root)

    def list_files(self, args: ListFilesInput) -> dict[str, Any]:
        root = self._resolve_inside_root(args.path)
        if not root.exists():
            raise ToolExecutionFailure(f"Path does not exist: {args.path}")
        if not root.is_dir():
            raise ToolExecutionFailure(f"Path is not a directory: {args.path}")

        files: list[str] = []
        sensitive_hidden_count = 0
        for path in sorted(root.rglob("*")):
            if self._should_skip(path):
                continue
            if len(path.relative_to(root).parts) > args.max_depth:
                continue
            if path.is_file():
                if self.sensitivity.is_sensitive(path):
                    sensitive_hidden_count += 1
                    continue
                files.append(self._relative(path))

        return {
            "root": self._relative(root),
            "files": files,
            "sensitive_hidden_count": sensitive_hidden_count,
        }

    def read_file(self, args: ReadFileInput) -> dict[str, Any]:
        path = self._resolve_inside_root(args.path)
        if not path.exists():
            raise ToolExecutionFailure(f"File does not exist: {args.path}")
        if not path.is_file():
            raise ToolExecutionFailure(f"Path is not a file: {args.path}")
        if self.sensitivity.is_sensitive(path):
            raise ToolExecutionFailure(
                "Sensitive path cannot be read by read-only tools.",
                code="sensitive_path_denied",
            )

        # Determine file size before reading to avoid loading arbitrarily
        # large files into memory. Falls back to a bounded read when the
        # size cannot be obtained (e.g. special files).
        size: int | None = None
        try:
            size = path.stat().st_size
        except OSError:
            size = None

        max_bytes = args.max_bytes
        if size is not None and size > max_bytes:
            # Read only the prefix we need; mark the result as truncated.
            with path.open("rb") as handle:
                chunk = handle.read(max_bytes + 1)
            truncated = len(chunk) > max_bytes
            chunk = chunk[:max_bytes]
            if self._looks_binary(chunk):
                raise ToolExecutionFailure(f"File appears to be binary: {args.path}")
            return {
                "path": self._relative(path),
                "content": chunk.decode("utf-8", errors="replace"),
                "truncated": truncated,
                "bytes_read": len(chunk),
                "bytes_total": size,
            }

        # Size is small enough (or unknown) - read everything, but cap the
        # read to a defensive upper bound when stat() failed so a streamed
        # or special file cannot exhaust memory.
        if size is None:
            with path.open("rb") as handle:
                raw = handle.read(max_bytes + 1)
            truncated = len(raw) > max_bytes
            chunk = raw[:max_bytes]
        else:
            raw = path.read_bytes()
            truncated = len(raw) > max_bytes
            chunk = raw[:max_bytes]
        if self._looks_binary(chunk):
            raise ToolExecutionFailure(f"File appears to be binary: {args.path}")

        return {
            "path": self._relative(path),
            "content": chunk.decode("utf-8", errors="replace"),
            "truncated": truncated,
            "bytes_read": len(chunk),
            "bytes_total": size if size is not None else len(raw),
        }

    def search_text(self, args: SearchTextInput) -> dict[str, Any]:
        start = self._resolve_inside_root(args.path)
        if not start.exists():
            raise ToolExecutionFailure(f"Path does not exist: {args.path}")

        files = [start] if start.is_file() else sorted(start.rglob("*"))
        needle = args.query if args.case_sensitive else args.query.lower()
        matches: list[dict[str, Any]] = []
        skipped_files: list[dict[str, Any]] = []

        for path in files:
            if len(matches) >= args.max_results:
                break
            if not path.is_file() or self._should_skip(path):
                continue
            if self.sensitivity.is_sensitive(path):
                continue
            # Bound scan cost: skip oversized files instead of reading them.
            try:
                file_size = path.stat().st_size
            except OSError:
                continue
            if file_size > args.max_file_bytes:
                skipped_files.append(
                    {
                        "path": self._relative(path),
                        "size": file_size,
                        "limit": args.max_file_bytes,
                    }
                )
                continue
            try:
                raw = path.read_bytes()
            except OSError:
                continue
            if self._looks_binary(raw[:4096]):
                continue
            text = raw.decode("utf-8", errors="replace")
            for line_number, line in enumerate(text.splitlines(), start=1):
                haystack = line if args.case_sensitive else line.lower()
                if needle in haystack:
                    matches.append(
                        {
                            "path": self._relative(path),
                            "line": line_number,
                            "text": redact_secret_text(line),
                        }
                    )
                    if len(matches) >= args.max_results:
                        break

        return {
            "query": args.query,
            "matches": matches,
            "truncated": len(matches) >= args.max_results,
            "skipped_files": skipped_files,
        }

    def _resolve_inside_root(self, user_path: str) -> Path:
        path = (self.project_root / user_path).resolve()
        if path != self.project_root and self.project_root not in path.parents:
            raise ToolExecutionFailure(
                f"Path escapes project root: {user_path}",
                code="validation_error",
            )
        return path

    def _relative(self, path: Path) -> str:
        if path == self.project_root:
            return "."
        return path.relative_to(self.project_root).as_posix()

    def _should_skip(self, path: Path) -> bool:
        try:
            parts = path.relative_to(self.project_root).parts
        except ValueError:
            return True
        return any(part in SKIP_DIRS for part in parts)

    @staticmethod
    def _looks_binary(raw: bytes) -> bool:
        return b"\x00" in raw


def register_read_only_tools(registry: Any) -> None:
    handlers = ReadOnlyToolHandlers(registry.project_root)
    registry.register(
        ToolSpec(
            name="list_files",
            version="0.0.4",
            description="List files inside the current project root.",
            input_model=ListFilesInput,
            handler=handlers.list_files,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.LIST_DIRECTORY,),
            operation=OperationKind.LIST_DIRECTORY,
            resource_resolver=lambda args, _root: [
                ResourceRef("directory", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=128),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
            cacheable=True,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="read_file",
            version="0.0.4",
            description="Read a UTF-8 text file inside the current project root.",
            input_model=ReadFileInput,
            handler=handlers.read_file,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=128),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
            cacheable=True,
            idempotent=True,
        )
    )
    registry.register(
        ToolSpec(
            name="search_text",
            version="0.0.4",
            description="Search for text in files inside the current project root.",
            input_model=SearchTextInput,
            handler=handlers.search_text,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda args, _root: [
                ResourceRef("directory", args.get("path") or ".", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=128),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
            cacheable=True,
            idempotent=True,
        )
    )
