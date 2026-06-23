from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any

from pydantic import BaseModel, ConfigDict, Field, create_model

from singularity.observability.models import TraceEventType
from singularity.plugins.models import (
    PLUGIN_TOOL_NAME_RE,
    PluginContributionSet,
    PluginManifest,
    PluginPermission,
    PluginToolContribution,
    PluginType,
)
from singularity.plugins.permissions import (
    ensure_permission_subset,
    permissions_for_tool,
)
from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools import (
    PermissionLevel,
    ToolCachePolicy,
    ToolExecutionBackendKind,
    ToolIdempotencyPolicy,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)


class PluginHost:
    def __init__(
        self,
        *,
        manifest: PluginManifest,
        manifest_hash: str,
        config: dict[str, Any],
        trace: Any | None = None,
    ) -> None:
        self._manifest = manifest
        self._manifest_hash = manifest_hash
        self._config = dict(config)
        self._trace = trace
        self._contributions = PluginContributionSet(plugin_id=manifest.id)

    @property
    def contributions(self) -> PluginContributionSet:
        return self._contributions

    def read_config(self) -> dict[str, Any]:
        return dict(self._config)

    def emit_trace(self, event: str, payload: dict[str, Any] | None = None) -> None:
        if self._trace is None or not hasattr(self._trace, "emit"):
            return
        self._trace.emit(
            TraceEventType.PLUGIN_EVENT,
            runtime="plugin",
            summary=f"Plugin {self._manifest.id} emitted {event}.",
            payload={
                "plugin_id": self._manifest.id,
                "plugin_version": self._manifest.version,
                "manifest_hash": self._manifest_hash,
                "plugin_event": event,
                "payload": payload or {},
            },
        )

    def register_tool(
        self,
        *,
        name: str,
        description: str,
        input_schema: dict[str, Any],
        handler: Callable[[Any], Any],
        risk_level: str,
        required_permissions: Iterable[PluginPermission | str] | None = None,
        version: str | None = None,
        output_model: type[BaseModel] | None = None,
        permission_level: PermissionLevel | str = PermissionLevel.READ_ONLY,
        capabilities: Iterable[Capability | str] = (),
        operation: OperationKind | str | None = None,
        resource_resolver: Callable[[dict[str, Any], Any], list[ResourceRef]] | None = None,
        side_effects: ToolSideEffectKind | str | None = None,
        sensitivity: ToolSensitivityLevel | str = ToolSensitivityLevel.WORKSPACE,
        timeout_seconds: float = 5.0,
        max_output_chars: int = 20000,
        cacheable: bool = False,
        idempotent: bool = True,
        execution_backend: ToolExecutionBackendKind | str = ToolExecutionBackendKind.IN_PROCESS,
        uses_edit_runtime: bool = False,
        uses_mutation_runtime: bool = False,
        uses_command_runtime: bool = False,
        delegates_policy_constraints: bool = False,
        risk_tags: Iterable[str] = (),
        approval_profile: dict[str, Any] | None = None,
        artifact_policy: dict[str, Any] | None = None,
    ) -> None:
        if self._manifest.type != PluginType.TOOL:
            raise ValueError("Only tool plugins may register tools.")
        if not PLUGIN_TOOL_NAME_RE.match(name):
            raise ValueError("Tool name must match ^[a-z][a-z0-9_]{0,63}$")
        exposed_name = f"{self._manifest.id}__{name}"
        permission_level = (
            permission_level
            if isinstance(permission_level, PermissionLevel)
            else PermissionLevel(str(permission_level))
        )
        capabilities_tuple = tuple(
            capability if isinstance(capability, Capability) else Capability(str(capability))
            for capability in capabilities
        )
        operation_value = (
            operation
            if operation is None or isinstance(operation, OperationKind)
            else OperationKind(str(operation))
        )
        side_effects_value = (
            side_effects
            if side_effects is None or isinstance(side_effects, ToolSideEffectKind)
            else ToolSideEffectKind(str(side_effects))
        )
        requested_permissions = permissions_for_tool(
            permission_level=permission_level,
            capabilities=capabilities_tuple,
            operation=operation_value,
            side_effects=side_effects_value,
            explicit=required_permissions,
        )
        requested_permissions = ensure_permission_subset(
            plugin_id=self._manifest.id,
            declared=self._manifest.permissions,
            requested=requested_permissions,
        )
        input_model = _model_from_schema(exposed_name, input_schema)
        plugin_profile = {
            "plugin_id": self._manifest.id,
            "plugin_version": self._manifest.version,
            "plugin_hash": self._manifest_hash,
            "local_tool_name": name,
            "risk_level": risk_level,
            "required_permissions": [permission.value for permission in requested_permissions],
        }
        spec = ToolSpec(
            name=exposed_name,
            version=version or self._manifest.version,
            description=description,
            input_model=input_model,
            output_model=output_model,
            handler=handler,
            permission_level=permission_level,
            risk_tags=(
                "plugin",
                f"plugin:{self._manifest.id}",
                f"risk:{risk_level}",
                *tuple(risk_tags),
            ),
            timeout_seconds=timeout_seconds,
            max_output_chars=max_output_chars,
            cacheable=cacheable,
            idempotent=idempotent,
            uses_edit_runtime=uses_edit_runtime,
            uses_mutation_runtime=uses_mutation_runtime,
            uses_command_runtime=uses_command_runtime,
            delegates_policy_constraints=delegates_policy_constraints,
            capabilities=capabilities_tuple,
            operation=operation_value,
            resource_resolver=resource_resolver,
            side_effects=side_effects_value,
            sensitivity=sensitivity,
            cache_policy=ToolCachePolicy(cacheable=cacheable),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=idempotent),
            execution_backend=execution_backend,
            approval_profile={**(approval_profile or {}), "plugin": plugin_profile},
            artifact_policy=artifact_policy or {},
        )
        self._contributions.tools.append(
            PluginToolContribution(
                plugin_id=self._manifest.id,
                local_name=name,
                exposed_name=exposed_name,
                required_permissions=requested_permissions,
                spec=spec,
            )
        )


def _model_from_schema(name: str, schema: dict[str, Any]) -> type[BaseModel]:
    if not isinstance(schema, dict):
        raise ValueError("input_schema must be an object.")
    if schema.get("type", "object") != "object":
        raise ValueError("input_schema root type must be object.")
    if schema.get("additionalProperties") is not False:
        raise ValueError("Plugin tool input_schema.additionalProperties must be false.")
    properties = schema.get("properties") or {}
    if not isinstance(properties, dict):
        raise ValueError("input_schema.properties must be an object.")
    required = set(schema.get("required") or [])
    fields: dict[str, tuple[Any, Any]] = {}
    for field_name, property_schema in properties.items():
        if not field_name.isidentifier():
            raise ValueError(f"input_schema property is not a valid identifier: {field_name}")
        property_schema = property_schema if isinstance(property_schema, dict) else {}
        annotation = _annotation_for_schema(property_schema)
        default = ... if field_name in required else property_schema.get("default", None)
        fields[field_name] = (
            annotation,
            Field(default, description=property_schema.get("description")),
        )
    config = ConfigDict(extra="forbid" if schema.get("additionalProperties") is False else "allow")
    return create_model(
        f"{name.title().replace('_', '')}Input",
        __config__=config,
        **fields,
    )


def _annotation_for_schema(schema: dict[str, Any]) -> Any:
    schema_type = schema.get("type")
    if isinstance(schema_type, list):
        non_null = [item for item in schema_type if item != "null"]
        if len(non_null) == 1:
            return _annotation_for_type(non_null[0]) | None
        return Any
    return _annotation_for_type(schema_type)


def _annotation_for_type(schema_type: str | None) -> Any:
    if schema_type == "string":
        return str
    if schema_type == "integer":
        return int
    if schema_type == "number":
        return float
    if schema_type == "boolean":
        return bool
    if schema_type == "array":
        return list[Any]
    if schema_type == "object":
        return dict[str, Any]
    return Any
