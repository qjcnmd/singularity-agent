from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.plugins.diagnostics import check_plugin, duplicate_plugin_ids, resolve_entrypoint_path
from singularity.plugins.discovery import discover_plugins
from singularity.plugins.loader import PluginLoader
from singularity.plugins.models import (
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginLockEntry,
    PluginPermission,
    PluginStatus,
    PluginToolContribution,
)
from singularity.plugins.permissions import ensure_permission_subset, permissions_for_tool
from singularity.plugins.status import PluginLockStore, PluginStatusStore
from singularity.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyRequest,
    PolicyEngine,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)
from singularity.release.paths import UserDataMode, UserDataPaths
from singularity.tools import ToolOrigin, ToolOriginKind, ToolRegistry


class PluginManager:
    def __init__(
        self,
        project_root: Path | str,
        *,
        user_data_paths: UserDataPaths | None = None,
        mode: UserDataMode | str | None = None,
        home: Path | str | None = None,
        trace: Any | None = None,
    ) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.user_data_paths = user_data_paths
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
            user_data_paths=self.user_data_paths,
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
        policy_engine: PolicyEngine | None = None,
    ) -> list[PluginDiagnostic]:
        discovered = self.discover()
        duplicates = duplicate_plugin_ids(discovered)
        diagnostics: list[PluginDiagnostic] = []
        lock_entries: list[PluginLockEntry] = []
        processed_enabled = False
        loader = PluginLoader(trace=self.trace)

        for plugin in discovered:
            raw_status = self.status_store.get(plugin.manifest.id)
            if raw_status is None or not raw_status.enabled:
                continue
            processed_enabled = True
            status = self.status_store.enabled_for(plugin)
            if status is None:
                plugin_diagnostics = check_plugin(plugin, status=raw_status)
                if not _has_error(plugin_diagnostics):
                    plugin_diagnostics.append(
                        PluginDiagnostic(
                            plugin_id=plugin.manifest.id,
                            severity=PluginDiagnosticSeverity.ERROR,
                            code="plugin_status_mismatch",
                            message="Enabled plugin status does not match the discovered manifest.",
                            path=str(plugin.manifest_path),
                            details={
                                "enabled_path": raw_status.path,
                                "enabled_manifest_hash": raw_status.manifest_hash,
                                "current_path": str(plugin.plugin_dir),
                                "current_manifest_hash": plugin.manifest_hash,
                            },
                        )
                    )
                diagnostics.extend(plugin_diagnostics)
                self._emit_check_failed(plugin, plugin_diagnostics)
                lock_entries.append(_lock_entry(plugin, enabled=False, compatibility_status="status_mismatch"))
                continue
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
            policy_diagnostic = self._policy_gate(plugin, policy_engine)
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
            registered_tool_count = 0
            for contribution in result.contribution_set.tools:
                admission_diagnostic = _admit_tool_contribution(
                    plugin,
                    status=status,
                    contribution=contribution,
                    policy_engine=policy_engine,
                )
                if admission_diagnostic is not None:
                    diagnostics.append(admission_diagnostic)
                    self._emit_check_failed(plugin, [admission_diagnostic])
                    continue
                try:
                    registry.register(
                        contribution.spec,
                        origin=_tool_origin(plugin, status=status, contribution=contribution),
                        admitted=True,
                        admission_reason="plugin_contribution_admitted",
                        metadata={
                            "manifest_path": str(plugin.manifest_path),
                            "plugin_policy_gate": policy_engine is not None,
                        },
                    )
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
                registered_tool_count += 1
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
                payload={
                    "tool_count": registered_tool_count,
                    "contribution_count": len(result.contribution_set.tools),
                },
            )
            lock_entries.append(_lock_entry(plugin, enabled=True, compatibility_status="compatible"))

        if processed_enabled or self.lock_store.path.exists():
            self.lock_store.write_entries(lock_entries)
        self.diagnostics = diagnostics
        return diagnostics

    def _policy_gate(
        self,
        plugin: DiscoveredPlugin,
        policy_engine: PolicyEngine | None,
    ) -> PluginDiagnostic | None:
        if policy_engine is None:
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
                component=PolicyComponent.SYSTEM,
                operation=OperationKind.READ_FILE,
                capability=capability,
                subject=PolicySubject(subject_type="component", name="PluginManager"),
                resource=resource,
                reason=f"Load local plugin {plugin.manifest.id}",
                proposed_by_model=False,
                risk_tags=["PLUGIN_LOAD"],
                metadata={"plugin_id": plugin.manifest.id, "source": plugin.source},
                touches_workspace=inside_workspace,
                workspace_root=str(self.project_root),
            )
            decision = policy_engine.enforce(request)
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
            component="plugin",
            summary=summary,
            payload={
                "plugin_id": plugin.manifest.id,
                "version": plugin.manifest.version,
                "manifest_hash": plugin.manifest_hash,
                **(payload or {}),
            },
            severity=severity,
        )


_HIGH_RISK_PLUGIN_PERMISSIONS = {
    PluginPermission.READ_OUTSIDE_WORKSPACE,
    PluginPermission.WRITE_WORKSPACE,
    PluginPermission.EXECUTE_COMMAND,
    PluginPermission.NETWORK_ACCESS,
    PluginPermission.READ_ENV,
    PluginPermission.CHANGE_CONFIG,
}


def _admit_tool_contribution(
    plugin: DiscoveredPlugin,
    *,
    status: PluginStatus,
    contribution: PluginToolContribution,
    policy_engine: PolicyEngine | None,
) -> PluginDiagnostic | None:
    spec = contribution.spec
    if contribution.plugin_id != plugin.manifest.id:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_identity_mismatch",
            "Plugin tool contribution plugin_id does not match the manifest.",
        )
    if contribution.exposed_name != spec.name:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_name_mismatch",
            "Plugin tool contribution exposed_name must match ToolSpec.name.",
        )
    try:
        required_permissions = ensure_permission_subset(
            plugin_id=plugin.manifest.id,
            declared=plugin.manifest.permissions,
            requested=contribution.required_permissions,
        )
    except Exception as exc:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_permission_not_declared",
            str(exc),
            details={"type": type(exc).__name__},
        )
    try:
        ensure_permission_subset(
            plugin_id=plugin.manifest.id,
            declared=status.approved_permissions,
            requested=required_permissions,
        )
    except Exception as exc:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_permission_not_approved",
            str(exc),
            details={"type": type(exc).__name__},
        )
    derived_permissions = permissions_for_tool(
        permission_level=spec.permission_level,
        capabilities=spec.capabilities,
        operation=spec.operation,
        side_effects=spec.side_effects,
    )
    missing_from_contribution = sorted(
        permission.value for permission in set(derived_permissions) - set(required_permissions)
    )
    if missing_from_contribution:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_permission_shape_mismatch",
            "ToolSpec permission shape requires permissions not declared by the contribution.",
            details={"missing_permissions": missing_from_contribution},
        )
    schema = spec.input_model.model_json_schema()
    if not isinstance(schema, dict) or schema.get("type", "object") != "object":
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_schema_invalid",
            "Plugin tool schema root must be an object.",
        )
    if schema.get("additionalProperties") is not False:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_schema_allows_extra_properties",
            "Plugin tool schema must forbid root additionalProperties.",
        )
    risk_tags = set(spec.risk_tags)
    if "plugin" not in risk_tags or f"plugin:{plugin.manifest.id}" not in risk_tags:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_risk_tags_missing",
            "Plugin tool must carry plugin risk tags.",
        )
    plugin_profile = spec.approval_profile.get("plugin")
    if not isinstance(plugin_profile, dict) or plugin_profile.get("plugin_id") != plugin.manifest.id:
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_approval_profile_missing",
            "Plugin tool must carry a plugin approval profile.",
        )
    if _has_high_risk_permissions(required_permissions) and not _has_high_risk_gate(
        policy_engine=policy_engine,
        approval_profile=spec.approval_profile,
    ):
        return _contribution_error(
            plugin,
            contribution,
            "plugin_tool_high_risk_gate_required",
            "High-risk plugin tool permissions require a policy gate or explicit approval profile.",
        )
    return None


def _tool_origin(
    plugin: DiscoveredPlugin,
    *,
    status: PluginStatus,
    contribution: PluginToolContribution,
) -> ToolOrigin:
    return ToolOrigin(
        kind=ToolOriginKind.PLUGIN,
        plugin_id=plugin.manifest.id,
        local_tool_name=contribution.local_name,
        exposed_name=contribution.exposed_name,
        manifest_hash=plugin.manifest_hash,
        source_path=_plugin_source_path(plugin),
        required_permissions=_permission_values(contribution.required_permissions),
        approved_permissions=_permission_values(status.approved_permissions),
        activation_hash=_stable_digest(plugin.manifest.activation),
        schema_digest=_stable_digest(contribution.spec.input_model.model_json_schema()),
    )


def _contribution_error(
    plugin: DiscoveredPlugin,
    contribution: PluginToolContribution,
    code: str,
    message: str,
    *,
    details: dict[str, Any] | None = None,
) -> PluginDiagnostic:
    return PluginDiagnostic(
        plugin_id=plugin.manifest.id,
        severity=PluginDiagnosticSeverity.ERROR,
        code=code,
        message=message,
        path=str(plugin.manifest_path),
        details={
            "tool_name": contribution.exposed_name,
            "local_tool_name": contribution.local_name,
            **(details or {}),
        },
    )


def _has_high_risk_permissions(permissions: tuple[PluginPermission, ...]) -> bool:
    return bool(set(permissions) & _HIGH_RISK_PLUGIN_PERMISSIONS)


def _has_high_risk_gate(
    *,
    policy_engine: PolicyEngine | None,
    approval_profile: dict[str, Any],
) -> bool:
    if policy_engine is not None:
        return True
    return bool(
        approval_profile.get("requires_approval")
        or approval_profile.get("approval_required")
        or approval_profile.get("requires_review")
        or approval_profile.get("plugin_high_risk_allowed")
    )


def _permission_values(permissions: tuple[PluginPermission, ...]) -> tuple[str, ...]:
    return tuple(permission.value for permission in permissions)


def _stable_digest(payload: Any) -> str:
    text = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _plugin_source_path(plugin: DiscoveredPlugin) -> str:
    try:
        entrypoint, _callable_name = resolve_entrypoint_path(plugin)
    except Exception:
        return str(plugin.manifest_path)
    return str(entrypoint)


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
