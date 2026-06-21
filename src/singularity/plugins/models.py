from __future__ import annotations

import re
from enum import Enum
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from singularity.tools import ToolSpec


API_VERSION = "1"
PLUGIN_ID_RE = re.compile(r"^[a-z][a-z0-9_]{1,63}$")
PLUGIN_TOOL_NAME_RE = re.compile(r"^[a-z][a-z0-9_]{0,63}$")


class PluginType(str, Enum):
    TOOL = "tool"
    PROVIDER = "provider"
    PROMPT = "prompt"
    MEMORY = "memory"
    EVAL = "eval"
    PROJECT_ADAPTER = "project_adapter"


class PluginPermission(str, Enum):
    READ_WORKSPACE = "read_workspace"
    READ_OUTSIDE_WORKSPACE = "read_outside_workspace"
    WRITE_WORKSPACE = "write_workspace"
    EXECUTE_COMMAND = "execute_command"
    NETWORK_ACCESS = "network_access"
    READ_ENV = "read_env"
    CHANGE_CONFIG = "change_config"


class PluginDiagnosticSeverity(str, Enum):
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"


class CompatibilitySpec(BaseModel):
    model_config = ConfigDict(extra="forbid")

    min_singularity_version: str | None = None
    max_singularity_version: str | None = None
    min_python: str | None = None
    max_python: str | None = None


class PluginManifest(BaseModel):
    model_config = ConfigDict(extra="forbid")

    id: str
    name: str
    version: str
    api_version: str
    entrypoint: str
    type: PluginType
    capabilities: tuple[str, ...]
    permissions: tuple[PluginPermission, ...]
    activation: dict[str, Any]
    compatibility: CompatibilitySpec
    config_schema: dict[str, Any]

    @field_validator("id")
    @classmethod
    def _valid_id(cls, value: str) -> str:
        if not PLUGIN_ID_RE.match(value):
            raise ValueError("plugin id must match ^[a-z][a-z0-9_]{1,63}$")
        return value

    @field_validator("entrypoint")
    @classmethod
    def _valid_entrypoint(cls, value: str) -> str:
        if ":" not in value:
            raise ValueError("entrypoint must use 'relative_file.py:function'")
        module_path, function_name = value.split(":", 1)
        if not module_path or not function_name:
            raise ValueError("entrypoint must include file and callable")
        if Path(module_path).is_absolute() or ".." in Path(module_path).parts:
            raise ValueError("entrypoint path must be relative and cannot escape plugin directory")
        if not function_name.isidentifier():
            raise ValueError("entrypoint callable must be a valid Python identifier")
        return value

    @model_validator(mode="after")
    def _valid_api(self) -> "PluginManifest":
        if self.api_version != API_VERSION:
            raise ValueError(f"api_version must be {API_VERSION}")
        return self


class DiscoveredPlugin(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    manifest: PluginManifest
    manifest_path: Path
    plugin_dir: Path
    source: str
    manifest_hash: str
    diagnostics: list["PluginDiagnostic"] = Field(default_factory=list)

    def to_summary(self, *, enabled: bool = False, compatibility_status: str = "unchecked") -> dict[str, Any]:
        return {
            "id": self.manifest.id,
            "name": self.manifest.name,
            "version": self.manifest.version,
            "api_version": self.manifest.api_version,
            "type": self.manifest.type.value,
            "source": self.source,
            "manifest_path": str(self.manifest_path),
            "plugin_dir": str(self.plugin_dir),
            "manifest_hash": self.manifest_hash,
            "enabled": enabled,
            "compatibility_status": compatibility_status,
            "diagnostics": [item.to_dict() for item in self.diagnostics],
        }


class PluginDiagnostic(BaseModel):
    model_config = ConfigDict(extra="forbid")

    plugin_id: str | None = None
    severity: PluginDiagnosticSeverity = PluginDiagnosticSeverity.ERROR
    code: str
    message: str
    path: str | None = None
    details: dict[str, Any] = Field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return self.model_dump(mode="json")


class PluginStatus(BaseModel):
    model_config = ConfigDict(extra="forbid")

    enabled: bool = False
    version: str | None = None
    path: str | None = None
    manifest_hash: str | None = None
    approved_permissions: tuple[PluginPermission, ...] = ()
    config: dict[str, Any] = Field(default_factory=dict)
    compatibility_status: str = "unchecked"


class PluginLockEntry(BaseModel):
    model_config = ConfigDict(extra="forbid")

    plugin_id: str
    version: str
    path: str
    manifest_hash: str
    compatibility_status: str
    enabled: bool


class PluginToolContribution(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    plugin_id: str
    local_name: str
    exposed_name: str
    required_permissions: tuple[PluginPermission, ...]
    spec: ToolSpec = Field(exclude=True)


class PluginContributionSet(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    plugin_id: str
    tools: list[PluginToolContribution] = Field(default_factory=list)
    provider: list[dict[str, Any]] = Field(default_factory=list)
    prompt: list[dict[str, Any]] = Field(default_factory=list)
    memory: list[dict[str, Any]] = Field(default_factory=list)
    eval: list[dict[str, Any]] = Field(default_factory=list)
    project_adapter: list[dict[str, Any]] = Field(default_factory=list)


class PluginLoadResult(BaseModel):
    model_config = ConfigDict(arbitrary_types_allowed=True)

    plugin_id: str
    loaded: bool
    contribution_set: PluginContributionSet | None = None
    diagnostics: list[PluginDiagnostic] = Field(default_factory=list)
