from __future__ import annotations

from pathlib import Path
from typing import Any

from singularity.plugins.compatibility import check_compatibility
from singularity.plugins.models import (
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginStatus,
)


def check_plugin(
    plugin: DiscoveredPlugin,
    *,
    status: PluginStatus | None = None,
) -> list[PluginDiagnostic]:
    diagnostics = list(plugin.diagnostics)
    diagnostics.extend(check_compatibility(plugin))
    diagnostics.extend(check_entrypoint_path(plugin))
    for diagnostic in validate_config(plugin.manifest.config_schema, status.config if status else {}):
        diagnostics.append(
            diagnostic.model_copy(
                update={"plugin_id": plugin.manifest.id, "path": str(plugin.manifest_path)}
            )
        )
    if status and status.enabled:
        diagnostics.extend(_status_diagnostics(plugin, status))
    return diagnostics


def check_entrypoint_path(plugin: DiscoveredPlugin) -> list[PluginDiagnostic]:
    try:
        resolve_entrypoint_path(plugin)
    except ValueError as exc:
        return [
            PluginDiagnostic(
                plugin_id=plugin.manifest.id,
                severity=PluginDiagnosticSeverity.ERROR,
                code="entrypoint_invalid",
                message=str(exc),
                path=str(plugin.manifest_path),
            )
        ]
    return []


def resolve_entrypoint_path(plugin: DiscoveredPlugin) -> tuple[Path, str]:
    entry_file, callable_name = plugin.manifest.entrypoint.split(":", 1)
    relative = Path(entry_file)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError("Plugin entrypoint must be a relative path inside the plugin directory.")
    if relative.suffix != ".py":
        raise ValueError("Plugin entrypoint must point to a Python .py file.")
    plugin_dir = plugin.plugin_dir.resolve(strict=True)
    entrypoint = (plugin.plugin_dir / relative).resolve(strict=True)
    try:
        entrypoint.relative_to(plugin_dir)
    except ValueError as exc:
        raise ValueError("Plugin entrypoint escapes the plugin directory.") from exc
    if not callable_name.isidentifier():
        raise ValueError("Plugin entrypoint callable must be a valid identifier.")
    return entrypoint, callable_name


def validate_config(schema: dict[str, Any], config: dict[str, Any]) -> list[PluginDiagnostic]:
    if not isinstance(config, dict):
        return [_config_error("config_invalid", "Plugin config must be an object.")]
    if not schema:
        return []
    if not isinstance(schema, dict):
        return [_config_error("config_schema_invalid", "config_schema must be an object.")]
    schema_type = schema.get("type", "object")
    if schema_type != "object":
        return [_config_error("config_schema_invalid", "config_schema root type must be object.")]

    diagnostics: list[PluginDiagnostic] = []
    properties = schema.get("properties") or {}
    if not isinstance(properties, dict):
        return [_config_error("config_schema_invalid", "config_schema.properties must be an object.")]
    required = set(schema.get("required") or [])
    for key in sorted(required):
        if key not in config:
            diagnostics.append(_config_error("config_required_missing", f"Missing required config key: {key}"))
    if schema.get("additionalProperties") is False:
        for key in sorted(set(config) - set(properties)):
            diagnostics.append(_config_error("config_unknown_key", f"Unknown config key: {key}"))
    for key, value in config.items():
        property_schema = properties.get(key)
        if not isinstance(property_schema, dict):
            continue
        expected = property_schema.get("type")
        if expected and not _matches_type(value, expected):
            diagnostics.append(
                _config_error(
                    "config_type_mismatch",
                    f"Config key {key} must be {expected}.",
                    details={"key": key, "expected": expected},
                )
            )
    return diagnostics


def _status_diagnostics(plugin: DiscoveredPlugin, status: PluginStatus) -> list[PluginDiagnostic]:
    diagnostics: list[PluginDiagnostic] = []
    if status.path and Path(status.path).resolve(strict=False) != plugin.plugin_dir.resolve(strict=False):
        diagnostics.append(
            PluginDiagnostic(
                plugin_id=plugin.manifest.id,
                severity=PluginDiagnosticSeverity.ERROR,
                code="status_path_mismatch",
                message="Enabled plugin path does not match the discovered manifest path.",
                path=str(plugin.manifest_path),
                details={"enabled_path": status.path},
            )
        )
    if status.manifest_hash and status.manifest_hash != plugin.manifest_hash:
        diagnostics.append(
            PluginDiagnostic(
                plugin_id=plugin.manifest.id,
                severity=PluginDiagnosticSeverity.ERROR,
                code="manifest_hash_mismatch",
                message="Plugin manifest changed since it was enabled; enable it again after review.",
                path=str(plugin.manifest_path),
            )
        )
    return diagnostics


def _matches_type(value: Any, expected: str | list[str]) -> bool:
    if isinstance(expected, list):
        return any(_matches_type(value, item) for item in expected)
    if expected == "string":
        return isinstance(value, str)
    if expected == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if expected == "number":
        return (isinstance(value, int | float)) and not isinstance(value, bool)
    if expected == "boolean":
        return isinstance(value, bool)
    if expected == "object":
        return isinstance(value, dict)
    if expected == "array":
        return isinstance(value, list)
    if expected == "null":
        return value is None
    return True


def _config_error(code: str, message: str, *, details: dict[str, Any] | None = None) -> PluginDiagnostic:
    return PluginDiagnostic(
        severity=PluginDiagnosticSeverity.ERROR,
        code=code,
        message=message,
        details=details or {},
    )


def duplicate_plugin_ids(discovered: list[DiscoveredPlugin]) -> set[str]:
    counts: dict[str, int] = {}
    for plugin in discovered:
        counts[plugin.manifest.id] = counts.get(plugin.manifest.id, 0) + 1
    return {plugin_id for plugin_id, count in counts.items() if count > 1}


def path_is_within(path: Path, parent: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(parent.resolve(strict=False))
        return True
    except ValueError:
        return False
