from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Callable

from pydantic import BaseModel, Field, ValidationError


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


class ToolRegistry:
    def __init__(self, project_root: Path) -> None:
        self.project_root = project_root.resolve()
        self._tools: dict[str, tuple[type[BaseModel], Callable[[BaseModel], dict[str, Any]]]] = {
            "list_files": (ListFilesInput, self._list_files),
            "read_file": (ReadFileInput, self._read_file),
            "search_text": (SearchTextInput, self._search_text),
        }

    def openai_tools(self) -> list[dict[str, Any]]:
        return [
            {
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": schema.model_json_schema(),
                },
            }
            for name, (schema, _handler) in self._tools.items()
            for description in (self._description_for(name),)
        ]

    def dispatch(self, tool_call: dict[str, Any]) -> dict[str, Any]:
        function = tool_call.get("function") or {}
        name = function.get("name")
        raw_arguments = function.get("arguments") or "{}"

        if name not in self._tools:
            return {"ok": False, "error": f"Unknown tool: {name}"}

        schema, handler = self._tools[name]
        try:
            arguments = json.loads(raw_arguments)
            validated = schema.model_validate(arguments)
            return handler(validated)
        except json.JSONDecodeError as exc:
            return {"ok": False, "error": f"Invalid JSON arguments: {exc}"}
        except ValidationError as exc:
            return {"ok": False, "error": exc.errors()}
        except Exception as exc:
            return {"ok": False, "error": str(exc)}

    def _list_files(self, data: BaseModel) -> dict[str, Any]:
        args = self._cast(data, ListFilesInput)
        root = self._resolve_inside_root(args.path)
        if not root.exists():
            return {"ok": False, "error": f"Path does not exist: {args.path}"}
        if not root.is_dir():
            return {"ok": False, "error": f"Path is not a directory: {args.path}"}

        files: list[str] = []
        for path in sorted(root.rglob("*")):
            if self._should_skip(path):
                continue
            if len(path.relative_to(root).parts) > args.max_depth:
                continue
            if path.is_file():
                files.append(self._relative(path))

        return {"ok": True, "root": self._relative(root), "files": files}

    def _read_file(self, data: BaseModel) -> dict[str, Any]:
        args = self._cast(data, ReadFileInput)
        path = self._resolve_inside_root(args.path)
        if not path.exists():
            return {"ok": False, "error": f"File does not exist: {args.path}"}
        if not path.is_file():
            return {"ok": False, "error": f"Path is not a file: {args.path}"}

        raw = path.read_bytes()
        truncated = len(raw) > args.max_bytes
        chunk = raw[: args.max_bytes]
        if self._looks_binary(chunk):
            return {"ok": False, "error": f"File appears to be binary: {args.path}"}

        return {
            "ok": True,
            "path": self._relative(path),
            "content": chunk.decode("utf-8", errors="replace"),
            "truncated": truncated,
            "bytes_read": len(chunk),
            "bytes_total": len(raw),
        }

    def _search_text(self, data: BaseModel) -> dict[str, Any]:
        args = self._cast(data, SearchTextInput)
        start = self._resolve_inside_root(args.path)
        if not start.exists():
            return {"ok": False, "error": f"Path does not exist: {args.path}"}

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
            "ok": True,
            "query": args.query,
            "matches": matches,
            "truncated": len(matches) >= args.max_results,
        }

    def _resolve_inside_root(self, user_path: str) -> Path:
        path = (self.project_root / user_path).resolve()
        if path != self.project_root and self.project_root not in path.parents:
            raise ValueError(f"Path escapes project root: {user_path}")
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

    @staticmethod
    def _description_for(name: str) -> str:
        descriptions = {
            "list_files": "List files inside the current project root.",
            "read_file": "Read a UTF-8 text file inside the current project root.",
            "search_text": "Search for text in files inside the current project root.",
        }
        return descriptions[name]

    @staticmethod
    def _cast(data: BaseModel, expected: type[BaseModel]) -> Any:
        if not isinstance(data, expected):
            raise TypeError(f"Expected {expected.__name__}, got {type(data).__name__}")
        return data
