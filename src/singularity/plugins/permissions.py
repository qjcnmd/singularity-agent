from __future__ import annotations

from collections.abc import Iterable
from typing import Any

from singularity.plugins.models import PluginPermission
from singularity.policy import Capability, OperationKind
from singularity.tools import PermissionLevel, ToolSideEffectKind


class PluginPermissionError(ValueError):
    pass


def coerce_permissions(values: Iterable[PluginPermission | str]) -> tuple[PluginPermission, ...]:
    return tuple(_coerce_permission(value) for value in values)


def ensure_permission_subset(
    *,
    plugin_id: str,
    declared: Iterable[PluginPermission | str],
    requested: Iterable[PluginPermission | str],
) -> tuple[PluginPermission, ...]:
    declared_set = set(coerce_permissions(declared))
    requested_set = set(coerce_permissions(requested))
    missing = sorted(permission.value for permission in requested_set - declared_set)
    if missing:
        raise PluginPermissionError(
            f"Plugin {plugin_id} did not declare required permissions: {', '.join(missing)}"
        )
    return tuple(sorted(requested_set, key=lambda permission: permission.value))


def permissions_for_tool(
    *,
    permission_level: PermissionLevel | str,
    capabilities: Iterable[Capability | str] = (),
    operation: OperationKind | str | None = None,
    side_effects: ToolSideEffectKind | str | None = None,
    explicit: Iterable[PluginPermission | str] | None = None,
) -> tuple[PluginPermission, ...]:
    permissions: set[PluginPermission] = set(coerce_permissions(explicit or ()))
    level = permission_level if isinstance(permission_level, PermissionLevel) else PermissionLevel(permission_level)
    if level == PermissionLevel.READ_ONLY:
        permissions.add(PluginPermission.READ_WORKSPACE)
    elif level == PermissionLevel.WRITE:
        permissions.add(PluginPermission.WRITE_WORKSPACE)
    elif level in {PermissionLevel.SHELL, PermissionLevel.GIT}:
        permissions.add(PluginPermission.EXECUTE_COMMAND)

    for capability in capabilities:
        permissions.update(_permissions_for_capability(capability))
    if operation is not None:
        permissions.update(_permissions_for_operation(operation))
    if side_effects is not None:
        permissions.update(_permissions_for_side_effect(side_effects))
    return tuple(sorted(permissions, key=lambda permission: permission.value))


def _coerce_permission(value: PluginPermission | str) -> PluginPermission:
    return value if isinstance(value, PluginPermission) else PluginPermission(str(value))


def _permissions_for_capability(value: Capability | str) -> set[PluginPermission]:
    capability = value if isinstance(value, Capability) else Capability(str(value))
    if capability in {Capability.READ_WORKSPACE, Capability.LIST_DIRECTORY}:
        return {PluginPermission.READ_WORKSPACE}
    if capability == Capability.READ_OUTSIDE_WORKSPACE:
        return {PluginPermission.READ_OUTSIDE_WORKSPACE}
    if capability in {
        Capability.MUTATE_WORKSPACE,
        Capability.CREATE_FILE,
        Capability.DELETE_FILE,
        Capability.MOVE_FILE,
        Capability.ROLLBACK_MUTATION,
    }:
        return {PluginPermission.WRITE_WORKSPACE}
    if capability in {
        Capability.EXECUTE_COMMAND,
        Capability.EXECUTE_PROJECT_CODE,
        Capability.EXECUTE_GENERATED_CODE,
        Capability.PACKAGE_INSTALL,
        Capability.PACKAGE_SCRIPT,
        Capability.START_LONG_PROCESS,
        Capability.KILL_PROCESS,
    }:
        return {PluginPermission.EXECUTE_COMMAND}
    if capability == Capability.NETWORK_ACCESS:
        return {PluginPermission.NETWORK_ACCESS}
    if capability in {Capability.READ_ENV, Capability.READ_SECRET}:
        return {PluginPermission.READ_ENV}
    if capability in {Capability.CHANGE_AGENT_CONFIG, Capability.WRITE_ENV}:
        return {PluginPermission.CHANGE_CONFIG}
    return set()


def _permissions_for_operation(value: OperationKind | str) -> set[PluginPermission]:
    operation = value if isinstance(value, OperationKind) else OperationKind(str(value))
    if operation in {OperationKind.READ_FILE, OperationKind.LIST_DIRECTORY, OperationKind.SEARCH}:
        return {PluginPermission.READ_WORKSPACE}
    if operation in {
        OperationKind.MUTATE_FILE,
        OperationKind.CREATE_FILE,
        OperationKind.DELETE_FILE,
        OperationKind.ROLLBACK,
    }:
        return {PluginPermission.WRITE_WORKSPACE}
    if operation in {
        OperationKind.EXECUTE_COMMAND,
        OperationKind.EXECUTE_PROJECT_CODE,
        OperationKind.PACKAGE_INSTALL,
        OperationKind.START_LONG_PROCESS,
        OperationKind.KILL_PROCESS,
    }:
        return {PluginPermission.EXECUTE_COMMAND}
    if operation == OperationKind.NETWORK_ACCESS:
        return {PluginPermission.NETWORK_ACCESS}
    if operation == OperationKind.READ_ENV:
        return {PluginPermission.READ_ENV}
    if operation == OperationKind.CHANGE_CONFIG:
        return {PluginPermission.CHANGE_CONFIG}
    return set()


def _permissions_for_side_effect(value: ToolSideEffectKind | str) -> set[PluginPermission]:
    side_effect = value if isinstance(value, ToolSideEffectKind) else ToolSideEffectKind(str(value))
    if side_effect == ToolSideEffectKind.READ_WORKSPACE:
        return {PluginPermission.READ_WORKSPACE}
    if side_effect == ToolSideEffectKind.MUTATE_WORKSPACE:
        return {PluginPermission.WRITE_WORKSPACE}
    if side_effect == ToolSideEffectKind.EXECUTE_COMMAND:
        return {PluginPermission.EXECUTE_COMMAND}
    if side_effect == ToolSideEffectKind.NETWORK:
        return {PluginPermission.NETWORK_ACCESS}
    return set()
