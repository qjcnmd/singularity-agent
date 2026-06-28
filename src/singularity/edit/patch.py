from __future__ import annotations

import ast
import re
from pathlib import Path
from typing import Any

from singularity.edit.models import (
    EditOperation,
    EditOperationKind,
    EditPlan,
    PatchCandidate,
)
from singularity.workspace.operations import (
    CreateFile,
    InsertAfter,
    InsertBefore,
    ReplaceRange,
    ReplaceText,
    UpdateJson,
)


class PatchBuildError(Exception):
    def __init__(self, code: str, message: str, *, path: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.path = path


class PatchBuilder:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)

    def build(self, plan: EditPlan) -> PatchCandidate:
        operations: list[Any] = []
        normalized_from: list[str] = []
        for operation in plan.operations:
            operations.extend(self._lower_operation(operation, plan=plan))
            normalized_from.append(operation.kind.value)
        touched_paths = _operation_paths(operations)
        return PatchCandidate(
            plan_id=plan.id,
            strategy=plan.strategy,
            operations=operations,
            touched_paths=touched_paths,
            normalized_from=normalized_from,
            metadata={"edit_operation_count": len(plan.operations)},
        )

    def _lower_operation(self, operation: EditOperation, *, plan: EditPlan) -> list[Any]:
        expected = operation.expected_sha256 or plan.scope.expected_hashes.get(operation.path)
        kind = operation.kind
        if kind == EditOperationKind.REPLACE_TEXT:
            return [
                ReplaceText(
                    path=operation.path,
                    old_text=_required(operation.old_text, "old_text", operation),
                    new_text=_coalesce(operation.new_text, operation.content, ""),
                    expected_sha256=expected,
                )
            ]
        if kind == EditOperationKind.INSERT_BEFORE:
            return [
                InsertBefore(
                    path=operation.path,
                    marker=_required(operation.marker, "marker", operation),
                    text=_coalesce(operation.text, operation.new_text, operation.content, ""),
                    expected_sha256=expected,
                )
            ]
        if kind == EditOperationKind.INSERT_AFTER:
            return [
                InsertAfter(
                    path=operation.path,
                    marker=_required(operation.marker, "marker", operation),
                    text=_coalesce(operation.text, operation.new_text, operation.content, ""),
                    expected_sha256=expected,
                )
            ]
        if kind == EditOperationKind.REPLACE_RANGE:
            if operation.start_line is None or operation.end_line is None:
                raise PatchBuildError(
                    "missing_line_range",
                    "replace_range requires start_line and end_line.",
                    path=operation.path,
                )
            return [
                ReplaceRange(
                    path=operation.path,
                    start_line=operation.start_line,
                    end_line=operation.end_line,
                    new_text=_coalesce(operation.new_text, operation.content, ""),
                    expected_sha256=expected,
                )
            ]
        if kind == EditOperationKind.CREATE_FILE:
            return [CreateFile(path=operation.path, content=_coalesce(operation.content, operation.new_text, ""))]
        if kind == EditOperationKind.REWRITE_FILE:
            content = _coalesce(operation.content, operation.new_text, "")
            full_path = self._full_path(operation.path)
            if full_path.exists():
                old_text = full_path.read_text(encoding="utf-8")
                return [
                    ReplaceText(
                        path=operation.path,
                        old_text=old_text,
                        new_text=content,
                        expected_sha256=expected,
                    )
                ]
            return [CreateFile(path=operation.path, content=content)]
        if kind == EditOperationKind.UPDATE_JSON:
            return [
                UpdateJson(
                    path=operation.path,
                    updates=dict(operation.updates or {}),
                    expected_sha256=expected,
                )
            ]
        if kind == EditOperationKind.REPLACE_SYMBOL:
            return [self._replace_python_symbol(operation, expected_sha256=expected)]
        if kind == EditOperationKind.REPLACE_IMPORT:
            return [self._replace_python_import(operation, expected_sha256=expected)]
        if kind == EditOperationKind.UNIFIED_DIFF:
            return [self._unified_diff_to_replace_text(operation, expected_sha256=expected)]
        raise PatchBuildError(
            "unsupported_edit_operation",
            f"Unsupported edit operation kind: {kind.value}",
            path=operation.path,
        )

    def _replace_python_symbol(
        self,
        operation: EditOperation,
        *,
        expected_sha256: str | None,
    ) -> ReplaceRange:
        if not operation.path.endswith(".py"):
            raise PatchBuildError("structured_edit_unsupported", "Python symbol edit requires a .py file.", path=operation.path)
        symbol_name = _required(operation.symbol_name, "symbol_name", operation)
        tree = ast.parse(self._read_text(operation.path), filename=operation.path)
        candidates: list[ast.AST] = []
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef | ast.ClassDef) and node.name == symbol_name:
                if operation.symbol_kind and _node_kind(node) != operation.symbol_kind:
                    continue
                candidates.append(node)
        if not candidates:
            raise PatchBuildError("structured_symbol_not_found", f"Symbol not found: {symbol_name}", path=operation.path)
        if len(candidates) > 1:
            raise PatchBuildError("structured_symbol_ambiguous", f"Symbol is ambiguous: {symbol_name}", path=operation.path)
        node = candidates[0]
        end_line = getattr(node, "end_lineno", None)
        if end_line is None:
            raise PatchBuildError("structured_symbol_no_range", f"Symbol has no source range: {symbol_name}", path=operation.path)
        return ReplaceRange(
            path=operation.path,
            start_line=int(node.lineno),
            end_line=int(end_line),
            new_text=_coalesce(operation.new_text, operation.content, ""),
            expected_sha256=expected_sha256,
        )

    def _replace_python_import(
        self,
        operation: EditOperation,
        *,
        expected_sha256: str | None,
    ) -> ReplaceRange:
        if not operation.path.endswith(".py"):
            raise PatchBuildError("structured_edit_unsupported", "Python import edit requires a .py file.", path=operation.path)
        import_name = _required(operation.import_name, "import_name", operation)
        tree = ast.parse(self._read_text(operation.path), filename=operation.path)
        candidates: list[ast.AST] = []
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                names = {alias.name for alias in node.names}
                if import_name in names:
                    candidates.append(node)
            elif isinstance(node, ast.ImportFrom):
                module = node.module or ""
                names = {alias.name for alias in node.names}
                if import_name == module or import_name in names or import_name == f"{module}.{next(iter(names), '')}":
                    candidates.append(node)
        if not candidates:
            raise PatchBuildError("structured_import_not_found", f"Import not found: {import_name}", path=operation.path)
        if len(candidates) > 1:
            raise PatchBuildError("structured_import_ambiguous", f"Import is ambiguous: {import_name}", path=operation.path)
        node = candidates[0]
        end_line = getattr(node, "end_lineno", None) or node.lineno
        return ReplaceRange(
            path=operation.path,
            start_line=int(node.lineno),
            end_line=int(end_line),
            new_text=_coalesce(operation.new_text, operation.content, ""),
            expected_sha256=expected_sha256,
        )

    def _unified_diff_to_replace_text(
        self,
        operation: EditOperation,
        *,
        expected_sha256: str | None,
    ) -> ReplaceText:
        current = self._read_text(operation.path)
        patched = _apply_single_file_unified_diff(
            current,
            _required(operation.diff, "diff", operation),
            path=operation.path,
        )
        return ReplaceText(
            path=operation.path,
            old_text=current,
            new_text=patched,
            expected_sha256=expected_sha256,
        )

    def _read_text(self, path: str) -> str:
        return self._full_path(path).read_text(encoding="utf-8")

    def _full_path(self, path: str) -> Path:
        full = (self.workspace_root / path).resolve(strict=False)
        try:
            full.relative_to(self.workspace_root)
        except ValueError as exc:
            raise PatchBuildError("path_out_of_scope", "Path is outside workspace.", path=path) from exc
        return full


def _apply_single_file_unified_diff(current: str, diff: str, *, path: str) -> str:
    before_lines = current.splitlines(keepends=True)
    result: list[str] = []
    source_index = 0
    hunk_seen = False
    for group in _parse_unified_hunks(diff):
        hunk_seen = True
        start = group["old_start"] - 1
        if start < source_index:
            raise PatchBuildError("unified_diff_overlap", "Unified diff hunks overlap.", path=path)
        result.extend(before_lines[source_index:start])
        source_index = start
        for line in group["lines"]:
            if line.startswith(" "):
                expected = line[1:]
                if source_index >= len(before_lines) or before_lines[source_index] != expected:
                    raise PatchBuildError("unified_diff_context_mismatch", "Unified diff context does not match.", path=path)
                result.append(before_lines[source_index])
                source_index += 1
            elif line.startswith("-"):
                expected = line[1:]
                if source_index >= len(before_lines) or before_lines[source_index] != expected:
                    raise PatchBuildError("unified_diff_context_mismatch", "Unified diff removal does not match.", path=path)
                source_index += 1
            elif line.startswith("+"):
                result.append(line[1:])
        if group["old_count"] == 0 and group["new_count"] == 0:
            raise PatchBuildError("unified_diff_empty_hunk", "Unified diff hunk is empty.", path=path)
    if not hunk_seen:
        raise PatchBuildError("unified_diff_no_hunks", "Unified diff contains no hunks.", path=path)
    result.extend(before_lines[source_index:])
    return "".join(result)


def _parse_unified_hunks(diff: str) -> list[dict[str, Any]]:
    hunks: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    header = re.compile(r"^@@ -(?P<old_start>\d+)(?:,(?P<old_count>\d+))? \+(?P<new_start>\d+)(?:,(?P<new_count>\d+))? @@")
    for line in diff.splitlines(keepends=True):
        if line.startswith(("--- ", "+++ ", "diff ", "index ")):
            continue
        match = header.match(line)
        if match:
            current = {
                "old_start": int(match.group("old_start")),
                "old_count": int(match.group("old_count") or 1),
                "new_start": int(match.group("new_start")),
                "new_count": int(match.group("new_count") or 1),
                "lines": [],
            }
            hunks.append(current)
            continue
        if current is None:
            continue
        if line.startswith((" ", "+", "-")):
            current["lines"].append(line)
    return hunks


def _operation_paths(operations: list[Any]) -> list[str]:
    paths: list[str] = []
    for operation in operations:
        path = getattr(operation, "path", None)
        if path:
            paths.append(str(path))
        new_path = getattr(operation, "new_path", None)
        if new_path:
            paths.append(str(new_path))
    return paths


def _required(value: str | None, field: str, operation: EditOperation) -> str:
    if value is None:
        raise PatchBuildError("missing_edit_field", f"{operation.kind.value} requires {field}.", path=operation.path)
    return value


def _coalesce(*values: str | None) -> str:
    for value in values:
        if value is not None:
            return value
    return ""


def _node_kind(node: ast.AST) -> str:
    if isinstance(node, ast.ClassDef):
        return "class"
    if isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
        return "function"
    return "unknown"
