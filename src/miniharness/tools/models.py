from __future__ import annotations

from enum import Enum
from pathlib import Path
from typing import Any, Callable

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from miniharness.policy import Capability, OperationKind, ResourceRef


class PermissionLevel(str, Enum):
    READ_ONLY = "read_only"
    WRITE = "write"
    SHELL = "shell"
    GIT = "git"


class ToolSideEffectKind(str, Enum):
    NONE = "none"
    READ_WORKSPACE = "read_workspace"
    MUTATE_WORKSPACE = "mutate_workspace"
    EXECUTE_COMMAND = "execute_command"
    NETWORK = "network"


class ToolSensitivityLevel(str, Enum):
    PUBLIC = "public"
    WORKSPACE = "workspace"
    SENSITIVE = "sensitive"
    SECRET = "secret"


class ToolExecutionBackendKind(str, Enum):
    IN_PROCESS = "in_process"
    DELEGATED_MUTATION_RUNTIME = "delegated_mutation_runtime"
    DELEGATED_COMMAND_RUNTIME = "delegated_command_runtime"
    DELEGATED_VERIFICATION_RUNTIME = "delegated_verification_runtime"
    EXTERNAL_PROCESS = "external_process"


class ToolCachePolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")

    cacheable: bool = False
    ttl_seconds: float | None = Field(None, gt=0)
    max_entries: int = Field(128, gt=0)


class ToolIdempotencyPolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")

    idempotent: bool = True
    replay_returns_previous: bool = True


class ToolRetryPolicy(BaseModel):
    model_config = ConfigDict(extra="forbid")

    max_attempts: int = Field(1, ge=1, le=5)


class ToolOutputEnvelope(BaseModel):
    model_config = ConfigDict(extra="forbid")

    content: Any
    sensitivity: ToolSensitivityLevel = ToolSensitivityLevel.WORKSPACE
    metadata: dict[str, Any] = Field(default_factory=dict)


class ToolInvocation(BaseModel):
    model_config = ConfigDict(extra="forbid")

    tool_call_id: str | None
    tool_name: str
    tool_version: str
    arguments: dict[str, Any]
    workspace_root: str


class ToolExecutionContext(BaseModel):
    model_config = ConfigDict(extra="forbid")

    workspace_root: str
    session_id: str
    task_id: str
    phase_id: str


class ToolExecutionRecord(BaseModel):
    model_config = ConfigDict(extra="forbid")

    invocation: ToolInvocation
    status: str
    error_code: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


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
    input_model: type[BaseModel] = Field(exclude=True)
    output_model: type[BaseModel] | None = Field(default=None, exclude=True)
    handler: Callable[[Any], Any] = Field(exclude=True)
    permission_level: PermissionLevel = PermissionLevel.READ_ONLY
    risk_tags: tuple[str, ...] = ()
    timeout_seconds: float = Field(5.0, gt=0)
    max_output_chars: int = Field(20000, gt=0)
    cacheable: bool = False
    idempotent: bool = True
    uses_mutation_runtime: bool = False
    uses_command_runtime: bool = False
    delegates_policy_constraints: bool = False
    capabilities: tuple[Capability, ...] = ()
    operation: OperationKind | None = None
    resource_resolver: Callable[[Any, Path], list[ResourceRef]] | None = Field(
        default=None,
        exclude=True,
    )
    side_effects: ToolSideEffectKind | str | None = None
    sensitivity: ToolSensitivityLevel | str = ToolSensitivityLevel.WORKSPACE
    cache_policy: ToolCachePolicy | None = None
    idempotency_policy: ToolIdempotencyPolicy | None = None
    retry_policy: ToolRetryPolicy = Field(default_factory=ToolRetryPolicy)
    execution_backend: ToolExecutionBackendKind | str = ToolExecutionBackendKind.IN_PROCESS
    approval_profile: dict[str, Any] = Field(default_factory=dict)
    artifact_policy: dict[str, Any] = Field(default_factory=dict)
    streamable: bool = False
    enabled: bool = True

    @field_validator("capabilities", mode="before")
    @classmethod
    def _coerce_capabilities(cls, value: Any) -> tuple[Capability, ...]:
        if value in (None, ()):
            return ()
        return tuple(Capability(item) if not isinstance(item, Capability) else item for item in value)

    @field_validator("operation", mode="before")
    @classmethod
    def _coerce_operation(cls, value: Any) -> OperationKind | None:
        if value is None:
            return None
        return value if isinstance(value, OperationKind) else OperationKind(value)

    @field_validator("side_effects", mode="before")
    @classmethod
    def _coerce_side_effects(cls, value: Any) -> ToolSideEffectKind | None:
        if value is None:
            return None
        return value if isinstance(value, ToolSideEffectKind) else ToolSideEffectKind(value)

    @field_validator("sensitivity", mode="before")
    @classmethod
    def _coerce_sensitivity(cls, value: Any) -> ToolSensitivityLevel:
        return value if isinstance(value, ToolSensitivityLevel) else ToolSensitivityLevel(value)

    @field_validator("execution_backend", mode="before")
    @classmethod
    def _coerce_backend(cls, value: Any) -> ToolExecutionBackendKind:
        return value if isinstance(value, ToolExecutionBackendKind) else ToolExecutionBackendKind(value)

    @model_validator(mode="after")
    def _apply_compatibility_defaults(self) -> "ToolSpec":
        if self.capabilities == ():
            self.capabilities = _default_capabilities(self.permission_level)
        if self.operation is None:
            self.operation = _default_operation(self.permission_level)
        if self.side_effects is None:
            self.side_effects = _default_side_effects(self.permission_level)
        if self.cache_policy is None:
            self.cache_policy = ToolCachePolicy(cacheable=self.cacheable)
        else:
            self.cacheable = self.cache_policy.cacheable
        if self.idempotency_policy is None:
            self.idempotency_policy = ToolIdempotencyPolicy(idempotent=self.idempotent)
        else:
            self.idempotent = self.idempotency_policy.idempotent
        return self


def _default_capabilities(permission: PermissionLevel) -> tuple[Capability, ...]:
    if permission == PermissionLevel.WRITE:
        return (Capability.MUTATE_WORKSPACE,)
    if permission == PermissionLevel.SHELL:
        return (Capability.EXECUTE_COMMAND,)
    if permission == PermissionLevel.GIT:
        return (Capability.EXECUTE_COMMAND,)
    return (Capability.READ_WORKSPACE,)


def _default_operation(permission: PermissionLevel) -> OperationKind:
    if permission == PermissionLevel.WRITE:
        return OperationKind.MUTATE_FILE
    if permission in {PermissionLevel.SHELL, PermissionLevel.GIT}:
        return OperationKind.EXECUTE_COMMAND
    return OperationKind.READ_FILE


def _default_side_effects(permission: PermissionLevel) -> ToolSideEffectKind:
    if permission == PermissionLevel.WRITE:
        return ToolSideEffectKind.MUTATE_WORKSPACE
    if permission in {PermissionLevel.SHELL, PermissionLevel.GIT}:
        return ToolSideEffectKind.EXECUTE_COMMAND
    return ToolSideEffectKind.READ_WORKSPACE
