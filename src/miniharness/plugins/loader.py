from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.plugins.diagnostics import resolve_entrypoint_path
from miniharness.plugins.host import PluginHost
from miniharness.plugins.models import (
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginLoadResult,
)


class PluginLoader:
    def __init__(self, *, trace: Any | None = None) -> None:
        self.trace = trace

    def load(
        self,
        plugin: DiscoveredPlugin,
        *,
        config: dict[str, Any],
    ) -> PluginLoadResult:
        self._emit(
            TraceEventType.PLUGIN_LOAD_STARTED,
            plugin,
            summary=f"Loading plugin {plugin.manifest.id}.",
        )
        try:
            entrypoint, callable_name = resolve_entrypoint_path(plugin)
            module = self._load_module(plugin, entrypoint)
            register = getattr(module, callable_name)
            if not callable(register):
                raise TypeError(f"Plugin entrypoint is not callable: {callable_name}")
            host = PluginHost(
                manifest=plugin.manifest,
                manifest_hash=plugin.manifest_hash,
                config=config,
                trace=self.trace,
            )
            register(host)
            self._emit(
                TraceEventType.PLUGIN_LOAD_COMPLETED,
                plugin,
                summary=f"Loaded plugin {plugin.manifest.id}.",
                payload={"tool_count": len(host.contributions.tools)},
            )
            return PluginLoadResult(
                plugin_id=plugin.manifest.id,
                loaded=True,
                contribution_set=host.contributions,
            )
        except Exception as exc:
            diagnostic = PluginDiagnostic(
                plugin_id=plugin.manifest.id,
                severity=PluginDiagnosticSeverity.ERROR,
                code="plugin_load_failed",
                message=str(exc),
                path=str(plugin.manifest_path),
                details={"type": type(exc).__name__},
            )
            self._emit(
                TraceEventType.PLUGIN_LOAD_FAILED,
                plugin,
                summary=f"Plugin {plugin.manifest.id} failed to load.",
                payload=diagnostic.to_dict(),
                severity=TraceSeverity.ERROR,
            )
            return PluginLoadResult(
                plugin_id=plugin.manifest.id,
                loaded=False,
                diagnostics=[diagnostic],
            )

    @staticmethod
    def _load_module(plugin: DiscoveredPlugin, entrypoint: Path) -> ModuleType:
        module_name = f"_miniharness_plugin_{plugin.manifest.id}_{plugin.manifest_hash[:12]}"
        spec = importlib.util.spec_from_file_location(module_name, entrypoint)
        if spec is None or spec.loader is None:
            raise ImportError(f"Cannot load plugin entrypoint: {entrypoint}")
        module = importlib.util.module_from_spec(spec)
        previous = sys.modules.get(module_name)
        sys.modules[module_name] = module
        try:
            spec.loader.exec_module(module)
        except Exception:
            if previous is None:
                sys.modules.pop(module_name, None)
            else:
                sys.modules[module_name] = previous
            raise
        return module

    def _emit(
        self,
        event_type: TraceEventType,
        plugin: DiscoveredPlugin,
        *,
        summary: str,
        payload: dict[str, Any] | None = None,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            runtime="plugin",
            summary=summary,
            payload={
                "plugin_id": plugin.manifest.id,
                "version": plugin.manifest.version,
                "manifest_hash": plugin.manifest_hash,
                **(payload or {}),
            },
            severity=severity,
        )
