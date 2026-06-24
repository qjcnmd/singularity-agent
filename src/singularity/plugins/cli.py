from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated, Any

import typer
from rich.console import Console
from rich.panel import Panel

from singularity.cli_paths import resolve_project_root
from singularity.observability import TraceRecorder
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.plugins.compatibility import check_compatibility
from singularity.plugins.diagnostics import check_plugin, duplicate_plugin_ids, validate_config
from singularity.plugins.discovery import discover_plugins
from singularity.plugins.models import (
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
)
from singularity.plugins.status import PluginStatusStore
from singularity.release.paths import UserDataMode, resolve_user_data_paths

plugin_app = typer.Typer(add_completion=False, no_args_is_help=True)
console = Console()
ProjectRootOption = Annotated[
    Path | None,
    typer.Option("--project-root", help="Workspace/project root; defaults to the current directory."),
]


@plugin_app.command("list")
def list_plugins(
    json_output: Annotated[
        bool,
        typer.Option("--json", help="Emit machine-readable JSON."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="User data mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override Singularity home for this command."),
    ] = None,
    project_root: ProjectRootOption = None,
) -> None:
    project_root = resolve_project_root(project_root)
    discovered = _discover(project_root, mode=mode, home=home)
    statuses = PluginStatusStore(project_root).load()
    payload = {
        "plugins": [
            plugin.to_summary(
                enabled=bool(statuses.get(plugin.manifest.id) and statuses[plugin.manifest.id].enabled),
                compatibility_status=statuses.get(plugin.manifest.id).compatibility_status
                if statuses.get(plugin.manifest.id)
                else "unchecked",
            )
            for plugin in discovered
        ]
    }
    _render(payload, json_output=json_output, title="plugins")


@plugin_app.command("inspect")
def inspect_plugin(
    plugin_id: str,
    json_output: Annotated[
        bool,
        typer.Option("--json", help="Emit machine-readable JSON."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="User data mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override Singularity home for this command."),
    ] = None,
    project_root: ProjectRootOption = None,
) -> None:
    project_root = resolve_project_root(project_root)
    discovered = _discover(project_root, mode=mode, home=home)
    plugin = _find_unique(discovered, plugin_id)
    status = PluginStatusStore(project_root).get(plugin_id)
    payload = plugin.to_summary(
        enabled=bool(status and status.enabled),
        compatibility_status=status.compatibility_status if status else "unchecked",
    )
    payload["manifest"] = plugin.manifest.model_dump(mode="json")
    payload["status"] = status.model_dump(mode="json") if status else None
    payload["diagnostics"] = [
        item.to_dict() for item in check_plugin(plugin, status=status)
    ]
    _render(payload, json_output=json_output, title=f"plugin {plugin_id}")


@plugin_app.command("enable")
def enable_plugin(
    plugin_id: str,
    config_json: Annotated[
        str | None,
        typer.Option("--config-json", help="Plugin config JSON object stored in project status."),
    ] = None,
    json_output: Annotated[
        bool,
        typer.Option("--json", help="Emit machine-readable JSON."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="User data mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override Singularity home for this command."),
    ] = None,
    project_root: ProjectRootOption = None,
) -> None:
    project_root = resolve_project_root(project_root)
    discovered = _discover(project_root, mode=mode, home=home)
    plugin = _find_unique(discovered, plugin_id)
    config = _parse_config(config_json)
    diagnostics = list(plugin.diagnostics)
    diagnostics.extend(check_compatibility(plugin))
    diagnostics.extend(
        diagnostic.model_copy(
            update={"plugin_id": plugin.manifest.id, "path": str(plugin.manifest_path)}
        )
        for diagnostic in validate_config(plugin.manifest.config_schema, config)
    )
    if _has_error(diagnostics):
        payload = {"ok": False, "plugin_id": plugin_id, "diagnostics": [item.to_dict() for item in diagnostics]}
        _render(payload, json_output=json_output, title="plugin enable failed")
        raise typer.Exit(1)
    status = PluginStatusStore(project_root).enable(
        plugin,
        config=config,
        compatibility_diagnostics=diagnostics,
    )
    _emit_management_trace(project_root, TraceEventType.PLUGIN_ENABLED, plugin)
    payload = {
        "ok": True,
        "plugin": plugin.to_summary(enabled=True, compatibility_status=status.compatibility_status),
        "status_path": str(PluginStatusStore(project_root).path),
    }
    _render(payload, json_output=json_output, title="plugin enabled")


@plugin_app.command("disable")
def disable_plugin(
    plugin_id: str,
    json_output: Annotated[
        bool,
        typer.Option("--json", help="Emit machine-readable JSON."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="User data mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override Singularity home for this command."),
    ] = None,
    project_root: ProjectRootOption = None,
) -> None:
    project_root = resolve_project_root(project_root)
    discovered = _discover(project_root, mode=mode, home=home)
    plugin = next((item for item in discovered if item.manifest.id == plugin_id), None)
    status = PluginStatusStore(project_root).disable(plugin_id)
    if plugin is not None:
        _emit_management_trace(project_root, TraceEventType.PLUGIN_DISABLED, plugin)
    payload = {"ok": True, "plugin_id": plugin_id, "status": status.model_dump(mode="json")}
    _render(payload, json_output=json_output, title="plugin disabled")


@plugin_app.command("check")
def check_plugins(
    plugin_id: Annotated[
        str | None,
        typer.Argument(help="Optional plugin id to check."),
    ] = None,
    json_output: Annotated[
        bool,
        typer.Option("--json", help="Emit machine-readable JSON."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="User data mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override Singularity home for this command."),
    ] = None,
    project_root: ProjectRootOption = None,
) -> None:
    project_root = resolve_project_root(project_root)
    discovered = _discover(project_root, mode=mode, home=home)
    if plugin_id:
        discovered = [_find_unique(discovered, plugin_id)]
    statuses = PluginStatusStore(project_root).load()
    duplicate_ids = duplicate_plugin_ids(discovered)
    results: list[dict[str, Any]] = []
    diagnostics: list[PluginDiagnostic] = []
    for plugin in discovered:
        plugin_diagnostics = check_plugin(plugin, status=statuses.get(plugin.manifest.id))
        if plugin.manifest.id in duplicate_ids:
            plugin_diagnostics.append(
                PluginDiagnostic(
                    plugin_id=plugin.manifest.id,
                    severity=PluginDiagnosticSeverity.ERROR,
                    code="duplicate_plugin_id",
                    message="Plugin id is not unique.",
                    path=str(plugin.manifest_path),
                )
            )
        diagnostics.extend(plugin_diagnostics)
        results.append(
            {
                **plugin.to_summary(
                    enabled=bool(statuses.get(plugin.manifest.id) and statuses[plugin.manifest.id].enabled),
                    compatibility_status=statuses.get(plugin.manifest.id).compatibility_status
                    if statuses.get(plugin.manifest.id)
                    else "unchecked",
                ),
                "diagnostics": [item.to_dict() for item in plugin_diagnostics],
            }
        )
    payload = {"ok": not _has_error(diagnostics), "plugins": results}
    _render(payload, json_output=json_output, title="plugin check")
    if _has_error(diagnostics):
        raise typer.Exit(1)


def _discover(
    project_root: Path,
    *,
    mode: UserDataMode | str | None,
    home: Path | str | None,
) -> list[DiscoveredPlugin]:
    return discover_plugins(
        project_root,
        user_data_paths=resolve_user_data_paths(mode=mode, home=home, project_root=project_root),
    )


def _find_unique(discovered: list[DiscoveredPlugin], plugin_id: str) -> DiscoveredPlugin:
    matches = [plugin for plugin in discovered if plugin.manifest.id == plugin_id]
    if not matches:
        console.print(f"Plugin not found: {plugin_id}", style="red")
        raise typer.Exit(1)
    if len(matches) > 1:
        console.print(f"Plugin id is ambiguous: {plugin_id}", style="red")
        raise typer.Exit(2)
    return matches[0]


def _parse_config(config_json: str | None) -> dict[str, Any]:
    if not config_json:
        return {}
    payload = json.loads(config_json)
    if not isinstance(payload, dict):
        raise typer.BadParameter("--config-json must be a JSON object.")
    return payload


def _emit_management_trace(
    project_root: Path,
    event_type: TraceEventType,
    plugin: DiscoveredPlugin,
) -> None:
    trace = TraceRecorder.create(project_root)
    trace.emit(
        event_type,
        component="plugin",
        summary=f"Plugin {plugin.manifest.id} management state changed.",
        payload={
            "plugin_id": plugin.manifest.id,
            "version": plugin.manifest.version,
            "manifest_hash": plugin.manifest_hash,
            "path": str(plugin.plugin_dir),
        },
        severity=TraceSeverity.INFO,
    )


def _has_error(diagnostics: list[PluginDiagnostic]) -> bool:
    return any(item.severity == PluginDiagnosticSeverity.ERROR for item in diagnostics)


def _render(payload: dict[str, Any], *, json_output: bool, title: str) -> None:
    text = json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)
    if json_output:
        typer.echo(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))
