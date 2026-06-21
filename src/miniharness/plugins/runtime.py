from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.plugins.diagnostics import check_plugin, duplicate_plugin_ids
from miniharness.plugins.discovery import discover_plugins
from miniharness.plugins.loader import PluginLoader
from miniharness.plugins.models import (
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginLockEntry,
)
from miniharness.plugins.status import PluginLockStore, PluginStatusStore
from miniharness.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)
from miniharness.release.paths import RuntimeMode, RuntimePaths
from miniharness.tools import ToolRegistry


class PluginRuntime:
    def __init__(
        self,
        project_root: Path | str,
        *,
        runtime_paths: RuntimePaths | None = None,
        mode: RuntimeMode | str | None = None,
        home: Path | str | None = None,
        trace: Any | None = None,
    ) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.runtime_paths = runtime_paths
        self.mode = mode
        self.home = home
        self.trace = trace
        self.status_store = PluginStatusStore(self.project_root)
        self.lock_store = PluginLockStore(self.project_root)
        self.diagnostics: list[PluginDiagnostic] = []
        self.discovered: list[DiscoveredPlugin] = []

    def discover(self) -> list[DiscoveredPlugin]:
        self.discovered = discover_plugins(
            self.project_root,
            runtime_paths=self.runtime_paths,
            mode=self.mode,
            home=self.home,
        )
        for plugin in self.discovered:
            self._emit(
                TraceEventType.PLUGIN_DISCOVERED,
                plugin,
                summary=f"Discovered plugin manifest {plugin.manifest.id}.",
                payload={"source": plugin.source},
            )
        return self.discovered

    def check(self) -> list[PluginDiagnostic]:
        discovered = self.discovered or self.discover()
        statuses = self.status_store.load()
        diagnostics: list[PluginDiagnostic] = []
        for plugin in discovered:
            diagnostics.extend(check_plugin(plugin, status=statuses.get(plugin.manifest.id)))
        self.diagnostics = diagnostics
        return diagnostics

    def activate(
        self,
        *,
        registry: ToolRegistry,
        policy_runtime: PolicyRuntime | None = None,
    ) -> list[PluginDiagnostic]:
        discovered = self.discover()
        duplicates = duplicate_plugin_ids(discovered)
        diagnostics: list[PluginDiagnostic] = []
        lock_entries: list[PluginLockEntry] = []
        processed_enabled = False
        loader = PluginLoader(trace=self.trace)

        for plugin in discovered:
            status = self.status_store.get(plugin.manifest.id)
            if status is None or not status.enabled:
                continue
            processed_enabled = True
            plugin_diagnostics = check_plugin(plugin, status=status)
            if plugin.manifest.id in duplicates:
                plugin_diagnostics.append(
                    PluginDiagnostic(
                        plugin_id=plugin.manifest.id,
                        severity=PluginDiagnosticSeverity.ERROR,
                        code="duplicate_plugin_id_enabled",
                        message="Enabled plugin id must resolve to a unique manifest.",
                        path=str(plugin.manifest_path),
                    )
                )
            policy_diagnostic = self._policy_gate(plugin, policy_runtime)
            if policy_diagnostic is not None:
                plugin_diagnostics.append(policy_diagnostic)
            if _has_error(plugin_diagnostics):
                diagnostics.extend(plugin_diagnostics)
                self._emit_check_failed(plugin, plugin_diagnostics)
                lock_entries.append(_lock_entry(plugin, enabled=True, compatibility_status="incompatible"))
                continue

            result = loader.load(plugin, config=status.config)
            if not result.loaded or result.contribution_set is None:
                diagnostics.extend(result.diagnostics)
                lock_entries.append(_lock_entry(plugin, enabled=True, compatibility_status="load_failed"))
                continue
            for contribution in result.contribution_set.tools:
                try:
                    registry.register(contribution.spec)
                except Exception as exc:
                    diagnostic = PluginDiagnostic(
                        plugin_id=plugin.manifest.id,
                        severity=PluginDiagnosticSeverity.ERROR,
                        code="tool_registration_failed",
                        message=str(exc),
                        path=str(plugin.manifest_path),
                        details={"tool_name": contribution.exposed_name, "type": type(exc).__name__},
                    )
                    diagnostics.append(diagnostic)
                    self._emit_check_failed(plugin, [diagnostic])
                    continue
                self._emit(
                    TraceEventType.PLUGIN_TOOL_REGISTERED,
                    plugin,
                    summary=f"Plugin tool registered: {contribution.exposed_name}.",
                    payload={
                        "tool_name": contribution.exposed_name,
                        "local_tool_name": contribution.local_name,
                        "required_permissions": [
                            permission.value for permission in contribution.required_permissions
                        ],
                    },
                )
            self._emit(
                TraceEventType.PLUGIN_ACTIVATED,
                plugin,
                summary=f"Activated plugin {plugin.manifest.id}.",
                payload={"tool_count": len(result.contribution_set.tools)},
            )
            lock_entries.append(_lock_entry(plugin, enabled=True, compatibility_status="compatible"))

        if processed_enabled or self.lock_store.path.exists():
            self.lock_store.write_entries(lock_entries)
        self.diagnostics = diagnostics
        return diagnostics

    def _policy_gate(
        self,
        plugin: DiscoveredPlugin,
        policy_runtime: PolicyRuntime | None,
    ) -> PluginDiagnostic | None:
        if policy_runtime is None:
            return None
        try:
            inside_workspace = plugin.plugin_dir.resolve(strict=False).is_relative_to(self.project_root)
            capability = Capability.READ_WORKSPACE if inside_workspace else Capability.READ_OUTSIDE_WORKSPACE
            resource = ResourceRef(
                "plugin",
                str(plugin.plugin_dir),
                normalized_identifier=str(plugin.plugin_dir.resolve(strict=False)),
                workspace_relative=inside_workspace,
            )
            request = PolicyRequest(
                session_id=getattr(self.trace, "session_id", "plugin_session"),
                task_id=getattr(self.trace, "run_id", "plugin_task"),
                phase_id="plugin_activation",
                action_id=f"plugin:{plugin.manifest.id}",
                runtime=RuntimeName.SYSTEM,
                operation=OperationKind.READ_FILE,
                capability=capability,
                subject=PolicySubject(subject_type="runtime", name="PluginRuntime"),
                resource=resource,
                reason=f"Load local plugin {plugin.manifest.id}",
                proposed_by_model=False,
                risk_tags=["PLUGIN_LOAD"],
                metadata={"plugin_id": plugin.manifest.id, "source": plugin.source},
                touches_workspace=inside_workspace,
                workspace_root=str(self.project_root),
            )
            decision = policy_runtime.enforce(request)
        except Exception as exc:
            return PluginDiagnostic(
                plugin_id=plugin.manifest.id,
                severity=PluginDiagnosticSeverity.ERROR,
                code="plugin_policy_gate_failed",
                message=str(exc),
                path=str(plugin.manifest_path),
                details={"type": type(exc).__name__},
            )
        if decision.outcome == DecisionOutcome.ALLOW:
            return None
        return PluginDiagnostic(
            plugin_id=plugin.manifest.id,
            severity=PluginDiagnosticSeverity.ERROR,
            code="plugin_policy_denied",
            message=decision.reason,
            path=str(plugin.manifest_path),
            details={"outcome": decision.outcome.value, "decision_id": decision.decision_id},
        )

    def _emit_check_failed(
        self,
        plugin: DiscoveredPlugin,
        diagnostics: list[PluginDiagnostic],
    ) -> None:
        self._emit(
            TraceEventType.PLUGIN_CHECK_FAILED,
            plugin,
            summary=f"Plugin {plugin.manifest.id} failed plugin gates.",
            payload={"diagnostics": [item.to_dict() for item in diagnostics]},
            severity=TraceSeverity.ERROR,
        )

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


def _has_error(diagnostics: list[PluginDiagnostic]) -> bool:
    return any(item.severity == PluginDiagnosticSeverity.ERROR for item in diagnostics)


def _lock_entry(
    plugin: DiscoveredPlugin,
    *,
    enabled: bool,
    compatibility_status: str,
) -> PluginLockEntry:
    return PluginLockEntry(
        plugin_id=plugin.manifest.id,
        version=plugin.manifest.version,
        path=str(plugin.plugin_dir),
        manifest_hash=plugin.manifest_hash,
        compatibility_status=compatibility_status,
        enabled=enabled,
    )
