from __future__ import annotations

from enum import Enum
from typing import Any, Callable

from pydantic import BaseModel, ConfigDict, Field


class PermissionLevel(str, Enum):
    READ_ONLY = "read_only"
    WRITE = "write"
    SHELL = "shell"
    GIT = "git"


class ToolError(BaseModel):
    code: str
    message: str
    details: Any | None = None


class ToolResult(BaseModel):
    ok: bool
    content: Any | None = None
    error_code: str | None = None
    error: ToolError | None = None
    truncated: bool = False
    metadata: dict[str, Any] = Field(default_factory=dict)

    @classmethod
    def success(
        cls,
        *,
        content: Any,
        truncated: bool = False,
        metadata: dict[str, Any] | None = None,
    ) -> "ToolResult":
        return cls(
            ok=True,
            content=content,
            truncated=truncated,
            metadata=metadata or {},
        )

    @classmethod
    def failure(
        cls,
        *,
        code: str,
        message: str,
        details: Any | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> "ToolResult":
        return cls(
            ok=False,
            error_code=code,
            error=ToolError(code=code, message=message, details=details),
            metadata=metadata or {},
        )


class ToolExecutionFailure(Exception):
    def __init__(
        self,
        message: str,
        *,
        code: str = "execution_error",
        details: Any | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details


class ToolSpec(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    name: str
    version: str = "0.0.1"
    description: str
    input_model: type[BaseModel]
    handler: Callable[[Any], Any]
    permission_level: PermissionLevel = PermissionLevel.READ_ONLY
    risk_tags: tuple[str, ...] = ()
    timeout_seconds: float = Field(5.0, gt=0)
    max_output_chars: int = Field(20000, gt=0)
    cacheable: bool = False
    idempotent: bool = True
    uses_mutation_runtime: bool = False
    uses_command_runtime: bool = False
