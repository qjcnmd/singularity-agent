from __future__ import annotations

import sys

from singularity.plugins.models import (
    API_VERSION,
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginManifest,
)
from singularity.release.metadata import package_version


def check_compatibility(plugin: DiscoveredPlugin | PluginManifest) -> list[PluginDiagnostic]:
    manifest = plugin.manifest if isinstance(plugin, DiscoveredPlugin) else plugin
    path = str(plugin.manifest_path) if isinstance(plugin, DiscoveredPlugin) else None
    diagnostics: list[PluginDiagnostic] = []
    if manifest.api_version != API_VERSION:
        diagnostics.append(
            _diagnostic(
                manifest.id,
                "api_version_incompatible",
                f"Plugin api_version {manifest.api_version!r} is not compatible with {API_VERSION!r}.",
                path=path,
            )
        )

    mh_version = _version_tuple(package_version())
    compatibility = manifest.compatibility
    if compatibility.min_singularity_version and mh_version < _version_tuple(
        compatibility.min_singularity_version
    ):
        diagnostics.append(
            _diagnostic(
                manifest.id,
                "singularity_version_too_old",
                "Current Singularity version is below plugin minimum.",
                path=path,
            )
        )
    if compatibility.max_singularity_version and mh_version > _version_tuple(
        compatibility.max_singularity_version
    ):
        diagnostics.append(
            _diagnostic(
                manifest.id,
                "singularity_version_too_new",
                "Current Singularity version is above plugin maximum.",
                path=path,
            )
        )

    py_version = sys.version_info[:3]
    if compatibility.min_python and py_version < _version_tuple(compatibility.min_python):
        diagnostics.append(
            _diagnostic(
                manifest.id,
                "python_version_too_old",
                "Current Python version is below plugin minimum.",
                path=path,
            )
        )
    if compatibility.max_python and py_version > _version_tuple(compatibility.max_python):
        diagnostics.append(
            _diagnostic(
                manifest.id,
                "python_version_too_new",
                "Current Python version is above plugin maximum.",
                path=path,
            )
        )
    return diagnostics


def compatibility_status(diagnostics: list[PluginDiagnostic]) -> str:
    return "incompatible" if any(item.severity == PluginDiagnosticSeverity.ERROR for item in diagnostics) else "compatible"


def _diagnostic(plugin_id: str, code: str, message: str, *, path: str | None) -> PluginDiagnostic:
    return PluginDiagnostic(
        plugin_id=plugin_id,
        severity=PluginDiagnosticSeverity.ERROR,
        code=code,
        message=message,
        path=path,
    )


def _version_tuple(value: str) -> tuple[int, ...]:
    parts: list[int] = []
    for token in value.replace("-", ".").split("."):
        digits = "".join(ch for ch in token if ch.isdigit())
        if digits == "":
            break
        parts.append(int(digits))
    return tuple(parts or [0])
