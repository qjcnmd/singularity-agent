from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4


@dataclass(frozen=True)
class ReplaceText:
    path: str
    old_text: str
    new_text: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class InsertBefore:
    path: str
    marker: str
    text: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class InsertAfter:
    path: str
    marker: str
    text: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class ReplaceRange:
    path: str
    start_line: int
    end_line: int
    new_text: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class ApplyUnifiedDiff:
    path: str
    diff: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class CreateFile:
    path: str
    content: str
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class DeleteFile:
    path: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class MoveFile:
    path: str
    new_path: str
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class UpdateJson:
    path: str
    updates: dict[str, Any]
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class UpdateYaml:
    path: str
    updates: dict[str, Any]
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class UpdateToml:
    path: str
    updates: dict[str, Any]
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


@dataclass(frozen=True)
class FormatFile:
    path: str
    formatter: str | None = None
    expected_sha256: str | None = None
    id: str = field(default_factory=lambda: uuid4().hex)


EditOperation = (
    ReplaceText
    | InsertBefore
    | InsertAfter
    | ReplaceRange
    | ApplyUnifiedDiff
    | CreateFile
    | DeleteFile
    | MoveFile
    | UpdateJson
    | UpdateYaml
    | UpdateToml
    | FormatFile
)


def operation_type(operation: EditOperation) -> str:
    return type(operation).__name__


def operation_paths(operation: EditOperation) -> list[str]:
    if isinstance(operation, MoveFile):
        return [operation.path, operation.new_path]
    return [operation.path]


def operation_expected_sha(operation: EditOperation) -> str | None:
    return getattr(operation, "expected_sha256", None)
