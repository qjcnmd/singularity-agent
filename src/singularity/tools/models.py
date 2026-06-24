from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from singularity.policy import Capability, OperationKind, ResourceRef


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
    DELEGATED_MUTATION_MANAGER = "delegated_mutation_manager"
    DELEGATED_EDIT_EXECUTOR = "delegated_edit_executor"
    DELEGATED_COMMAND_EXECUTOR = "delegated_command_executor"
    DELEGATED_VERIFICATION_RUNNER = "delegated_verification_runner"
    EXTERNAL_PROCESS = "external_process"


class ToolOriginKind(str, Enum):
    BUILTIN = "builtin"
    PLUGIN = "plugin"
    FUTURE_MCP = "future_mcp"


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


class ToolOrigin(BaseModel):
    model_config = ConfigDict(extra="forbid")

    kind: ToolOriginKind = ToolOriginKind.BUILTIN
    plugin_id: str | None = None
    local_tool_name: str | None = None
    exposed_name: str | None = None
    manifest_hash: str | None = None
    source_path: str | None = None
    required_permissions: tuple[str, ...] = ()
    approved_permissions: tuple[str, ...] = ()
    activation_hash: str | None = None
    schema_digest: str | None = None


@dataclass(frozen=True)
class RegisteredToolRecord:
    spec: "ToolSpec"
    origin: ToolOrigin = field(default_factory=ToolOrigin)
    admitted: bool = True
    admission_reason: str = "registered"
    diagnostics: tuple[str, ...] = ()
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class ToolExecutionRequest:
    tool_call_id: str | None
    tool_name: str
    raw_arguments: str
    normalized_arguments: dict[str, Any] = field(default_factory=dict)
    batch_id: str | None = None
    run_id: str | None = None
    session_id: str | None = None
    task_id: str | None = None
    phase_id: str | None = None
    model_request_id: str | None = None
    model_response_id: str | None = None
    argument_digest: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.tool_name = str(self.tool_name or "<unknown>")
        self.raw_arguments = _raw_arguments_text(self.raw_arguments)
        self.normalized_arguments = dict(self.normalized_arguments or {})
        self.metadata = dict(self.metadata or {})
        if not self.argument_digest:
            self.argument_digest = _execution_argument_digest(
                self.normalized_arguments if self.normalized_arguments else self.raw_arguments
            )

    @classmethod
    def from_provider_tool_call(cls, tool_call: dict[str, Any]) -> "ToolExecutionRequest":
        function = tool_call.get("function") if isinstance(tool_call, dict) else {}
        function = function if isinstance(function, dict) else {}
        return cls(
            tool_call_id=str(tool_call.get("id") or "") if isinstance(tool_call, dict) else None,
            tool_name=str(function.get("name") or "<unknown>"),
            raw_arguments=_raw_arguments_text(function.get("arguments") or "{}"),
        )

    @classmethod
    def from_envelope(
        cls,
        envelope: Any,
        *,
        batch: Any | None = None,
    ) -> "ToolExecutionRequest":
        metadata = dict(getattr(envelope, "metadata", {}) or {})
        batch_id = getattr(batch, "batch_id", None) or metadata.get("batch_id")
        return cls(
            tool_call_id=getattr(envelope, "tool_call_id", None),
            tool_name=str(getattr(envelope, "tool_name", "") or "<unknown>"),
            raw_arguments=_raw_arguments_text(getattr(envelope, "raw_arguments", "{}")),
            normalized_arguments=dict(getattr(envelope, "normalized_arguments", {}) or {}),
            batch_id=str(batch_id) if batch_id else None,
            run_id=str(getattr(envelope, "run_id", "") or getattr(batch, "run_id", "") or "") or None,
            session_id=str(getattr(envelope, "session_id", "") or getattr(batch, "session_id", "") or "") or None,
            task_id=str(getattr(envelope, "task_id", "") or getattr(batch, "task_id", "") or "") or None,
            phase_id=str(getattr(envelope, "phase_id", "") or getattr(batch, "phase_id", "") or "") or None,
            model_request_id=str(
                getattr(envelope, "model_request_id", "")
                or getattr(batch, "model_request_id", "")
                or ""
            )
            or None,
            model_response_id=str(
                getattr(envelope, "model_response_id", "")
                or getattr(batch, "model_response_id", "")
                or ""
            )
            or None,
            argument_digest=str(getattr(envelope, "argument_digest", "") or ""),
            metadata=metadata,
        )


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
    uses_edit_executor: bool = False
    uses_mutation_manager: bool = False
    uses_command_executor: bool = False
    delegates_policy_constraints: bool = False
    capabilities: tuple[Capability, ...] = ()
    operation: OperationKind | None = None
    resource_resolver: Callable[[Any, Path], list[ResourceRef]] | None = Field(
        default=None,
        exclude=True,
    )
    side_effects: ToolSideEffectKind | None = None
    sensitivity: ToolSensitivityLevel = ToolSensitivityLevel.WORKSPACE
    cache_policy: ToolCachePolicy | None = None
    idempotency_policy: ToolIdempotencyPolicy | None = None
    retry_policy: ToolRetryPolicy = Field(default_factory=ToolRetryPolicy)
    execution_backend: ToolExecutionBackendKind = ToolExecutionBackendKind.IN_PROCESS
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


def _raw_arguments_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value if value is not None else {}, ensure_ascii=False, sort_keys=True, default=str)


def _execution_argument_digest(value: Any) -> str:
    text = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
