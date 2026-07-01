from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import (
    PermissionLevel,
    ToolCachePolicy,
    ToolExecutionFailure,
    ToolIdempotencyPolicy,
    ToolSensitivityLevel,
    ToolSideEffectKind,
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
READ_ONLY_TOOL_TIMEOUT_SECONDS = 10.0


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
    line_start: int | None = Field(
        None,
        ge=1,
        description="Optional 1-based first line to read.",
    )
    line_count: int | None = Field(
        None,
        ge=1,
        le=1000,
        description="Optional number of lines to read from line_start.",
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
    def __init__(self, project_root: Path, *, permission_profile: Any = None) -> None:
        self.project_root = project_root.resolve()
        self.permission_profile = permission_profile
        self.sensitivity = FileSensitivityClassifier(self.project_root)

    def list_files(self, args: ListFilesInput) -> dict[str, Any]:
        root = self._resolve_inside_root(args.path)
        self._ensure_not_protected(root)
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
                if self._is_protected(path):
                    sensitive_hidden_count += 1
                    continue
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
        self._ensure_not_protected(path)
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
        if args.line_start is not None or args.line_count is not None:
            return self._read_file_line_window(
                path,
                user_path=args.path,
                max_bytes=max_bytes,
                line_start=args.line_start or 1,
                line_count=args.line_count or 200,
                size=size,
            )

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

    def _read_file_line_window(
        self,
        path: Path,
        *,
        user_path: str,
        max_bytes: int,
        line_start: int,
        line_count: int,
        size: int | None,
    ) -> dict[str, Any]:
        with path.open("rb") as handle:
            sample = handle.read(4096)
        if self._looks_binary(sample):
            raise ToolExecutionFailure(f"File appears to be binary: {user_path}")

        selected: list[str] = []
        total_lines = 0
        window_end = line_start + line_count - 1
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for current_line, line in enumerate(handle, start=1):
                total_lines = current_line
                if line_start <= current_line <= window_end:
                    selected.append(line.rstrip("\r\n"))

        content = "\n".join(selected)
        encoded = content.encode("utf-8", errors="replace")
        truncated_by_bytes = len(encoded) > max_bytes
        if truncated_by_bytes:
            content = encoded[:max_bytes].decode("utf-8", errors="replace")
            encoded = content.encode("utf-8", errors="replace")
        line_end = line_start + len(selected) - 1 if selected else line_start - 1
        has_more_lines = window_end < total_lines

        return {
            "path": self._relative(path),
            "content": content,
            "truncated": truncated_by_bytes,
            "bytes_read": len(encoded),
            "bytes_total": size if size is not None else len(encoded),
            "line_start": line_start,
            "line_end": line_end,
            "line_count": len(selected),
            "total_lines": total_lines,
            "has_more_lines": has_more_lines,
        }

    def search_text(self, args: SearchTextInput) -> dict[str, Any]:
        start = self._resolve_inside_root(args.path)
        self._ensure_not_protected(start)
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
            if self._is_protected(path):
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
        raw = Path(user_path).expanduser()
        path = (raw if raw.is_absolute() else self.project_root / raw).resolve()
        roots = (
            self.permission_profile.workspace_roots
            if self.permission_profile is not None
            else (self.project_root,)
        )
        roots = (
            *roots,
            *(
                self.permission_profile.additional_writable_directories
                if self.permission_profile is not None
                else ()
            ),
        )
        if not any(path == root or root in path.parents for root in roots):
            raise ToolExecutionFailure(
                f"Path escapes project root: {user_path}",
                code="validation_error",
            )
        return path

    def _relative(self, path: Path) -> str:
        if path == self.project_root:
            return "."
        try:
            return path.relative_to(self.project_root).as_posix()
        except ValueError:
            for index, root in enumerate(
                self.permission_profile.additional_writable_directories
                if self.permission_profile is not None
                else ()
            ):
                try:
                    relative = path.relative_to(root).as_posix()
                except ValueError:
                    continue
                return f"additional-dir:{index}/{relative or '.'}"
            return path.name

    def _is_protected(self, path: Path) -> bool:
        return bool(
            self.permission_profile is not None
            and self.permission_profile.matching_protected_rule(path, access="read")
        )

    def _ensure_not_protected(self, path: Path) -> None:
        if self._is_protected(path):
            raise ToolExecutionFailure(
                "Protected path cannot be read by model-visible tools.",
                code="protected_path_denied",
            )

    def _should_skip(self, path: Path) -> bool:
        roots = (
            self.project_root,
            *(
                self.permission_profile.additional_writable_directories
                if self.permission_profile is not None
                else ()
            ),
        )
        parts = None
        for root in roots:
            try:
                parts = path.relative_to(root).parts
                break
            except ValueError:
                continue
        if parts is None:
            return True
        return any(part in SKIP_DIRS for part in parts)

    @staticmethod
    def _looks_binary(raw: bytes) -> bool:
        return b"\x00" in raw


def register_read_only_tools(registry: Any) -> None:
    handlers = ReadOnlyToolHandlers(
        registry.project_root,
        permission_profile=getattr(registry, "permission_profile", None),
    )
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
            timeout_seconds=READ_ONLY_TOOL_TIMEOUT_SECONDS,
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
            version="0.0.5",
            description="Read a UTF-8 text file inside the current project root, optionally by line window.",
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
            timeout_seconds=READ_ONLY_TOOL_TIMEOUT_SECONDS,
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
            timeout_seconds=READ_ONLY_TOOL_TIMEOUT_SECONDS,
            max_output_chars=20000,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=128),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
            cacheable=True,
            idempotent=True,
        )
    )
