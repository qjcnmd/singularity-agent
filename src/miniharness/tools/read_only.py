from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, Field

from miniharness.tools.models import (
    PermissionLevel,
    ToolExecutionFailure,
    ToolSpec,
)


SKIP_DIRS = {
    ".git",
    ".miniharness",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
}


class ListFilesInput(BaseModel):
    path: str = Field(".", description="Directory to list, relative to project root.")
    max_depth: int = Field(4, ge=0, le=10, description="Maximum directory depth.")


class ReadFileInput(BaseModel):
    path: str = Field(..., description="File path, relative to project root.")
    max_bytes: int = Field(
        20000, ge=1, le=200000, description="Maximum bytes to read."
    )


class SearchTextInput(BaseModel):
    query: str = Field(..., min_length=1, description="Text to search for.")
    path: str = Field(".", description="File or directory path to search.")
    case_sensitive: bool = Field(False, description="Whether matching is case-sensitive.")
    max_results: int = Field(50, ge=1, le=200, description="Maximum matches to return.")


class ReadOnlyToolHandlers:
    def __init__(self, project_root: Path) -> None:
        self.project_root = project_root.resolve()

    def list_files(self, args: ListFilesInput) -> dict[str, Any]:
        root = self._resolve_inside_root(args.path)
        if not root.exists():
            raise ToolExecutionFailure(f"Path does not exist: {args.path}")
        if not root.is_dir():
            raise ToolExecutionFailure(f"Path is not a directory: {args.path}")

        files: list[str] = []
        for path in sorted(root.rglob("*")):
            if self._should_skip(path):
                continue
            if len(path.relative_to(root).parts) > args.max_depth:
                continue
            if path.is_file():
                files.append(self._relative(path))

        return {"root": self._relative(root), "files": files}

    def read_file(self, args: ReadFileInput) -> dict[str, Any]:
        path = self._resolve_inside_root(args.path)
        if not path.exists():
            raise ToolExecutionFailure(f"File does not exist: {args.path}")
        if not path.is_file():
            raise ToolExecutionFailure(f"Path is not a file: {args.path}")

        raw = path.read_bytes()
        truncated = len(raw) > args.max_bytes
        chunk = raw[: args.max_bytes]
        if self._looks_binary(chunk):
            raise ToolExecutionFailure(f"File appears to be binary: {args.path}")

        return {
            "path": self._relative(path),
            "content": chunk.decode("utf-8", errors="replace"),
            "truncated": truncated,
            "bytes_read": len(chunk),
            "bytes_total": len(raw),
        }

    def search_text(self, args: SearchTextInput) -> dict[str, Any]:
        start = self._resolve_inside_root(args.path)
        if not start.exists():
            raise ToolExecutionFailure(f"Path does not exist: {args.path}")

        files = [start] if start.is_file() else sorted(start.rglob("*"))
        needle = args.query if args.case_sensitive else args.query.lower()
        matches: list[dict[str, Any]] = []

        for path in files:
            if len(matches) >= args.max_results:
                break
            if not path.is_file() or self._should_skip(path):
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
                            "text": line,
                        }
                    )
                    if len(matches) >= args.max_results:
                        break

        return {
            "query": args.query,
            "matches": matches,
            "truncated": len(matches) >= args.max_results,
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
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
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
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
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
            risk_tags=("read", "filesystem"),
            timeout_seconds=5.0,
            max_output_chars=20000,
            cacheable=True,
            idempotent=True,
        )
    )
