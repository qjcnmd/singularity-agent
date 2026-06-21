from __future__ import annotations

import hashlib
import os
import tomllib
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from miniharness.plugins.models import (
    CompatibilitySpec,
    DiscoveredPlugin,
    PluginDiagnostic,
    PluginDiagnosticSeverity,
    PluginManifest,
    PluginType,
)
from miniharness.release.paths import RuntimeMode, RuntimePaths, resolve_runtime_paths

MANIFEST_NAMES = ("plugin.toml", "miniharness-plugin.toml")
ENV_PLUGIN_PATH = "MINIHARNESS_PLUGIN_PATH"


def discover_plugins(
    project_root: Path | str,
    *,
    runtime_paths: RuntimePaths | None = None,
    mode: RuntimeMode | str | None = None,
    home: Path | str | None = None,
) -> list[DiscoveredPlugin]:
    """Discover plugin manifests without importing plugin code."""

    project_root = Path(project_root).resolve(strict=False)
    paths = runtime_paths or resolve_runtime_paths(
        mode=mode,
        home=home,
        project_root=project_root,
    )
    discovered: list[DiscoveredPlugin] = []
    for source, root in _discovery_roots(project_root, paths):
        for manifest_path in _manifest_paths(root):
            discovered.append(_read_manifest(manifest_path, source=source))
    return _mark_duplicate_ids(discovered)


def _discovery_roots(
    project_root: Path,
    runtime_paths: RuntimePaths,
) -> list[tuple[str, Path]]:
    roots: list[tuple[str, Path]] = [
        ("project", project_root / ".miniharness" / "plugins"),
    ]
    for raw in os.getenv(ENV_PLUGIN_PATH, "").split(os.pathsep):
        if raw.strip():
            roots.append(("env", Path(raw).expanduser()))
    roots.append(("user", runtime_paths.config_dir / "plugins"))
    return roots


def _manifest_paths(root: Path) -> list[Path]:
    if not root.exists():
        return []
    candidates: list[Path] = []
    for name in MANIFEST_NAMES:
        path = root / name
        if path.is_file():
            candidates.append(path)
    if root.is_dir():
        for child in sorted(root.iterdir(), key=lambda item: item.name):
            if not child.is_dir():
                continue
            for name in MANIFEST_NAMES:
                path = child / name
                if path.is_file():
                    candidates.append(path)
                    break
    return candidates


def _read_manifest(manifest_path: Path, *, source: str) -> DiscoveredPlugin:
    raw: dict[str, Any] = {}
    diagnostics: list[PluginDiagnostic] = []
    manifest: PluginManifest | None = None
    try:
        raw = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        manifest = PluginManifest.model_validate(raw)
    except (tomllib.TOMLDecodeError, ValidationError, OSError, ValueError) as exc:
        diagnostics.append(
            PluginDiagnostic(
                plugin_id=str(raw.get("id")) if isinstance(raw, dict) and raw.get("id") else None,
                severity=PluginDiagnosticSeverity.ERROR,
                code="manifest_invalid",
                message=str(exc),
                path=str(manifest_path),
            )
        )
        manifest = _invalid_manifest(raw, manifest_path)
    return DiscoveredPlugin(
        manifest=manifest,
        manifest_path=manifest_path.resolve(strict=False),
        plugin_dir=manifest_path.parent.resolve(strict=False),
        source=source,
        manifest_hash=_sha256_file(manifest_path),
        diagnostics=diagnostics,
    )


def _invalid_manifest(raw: dict[str, Any], manifest_path: Path) -> PluginManifest:
    fallback_id = raw.get("id") if isinstance(raw, dict) else None
    fallback_name = raw.get("name") if isinstance(raw, dict) else None
    safe_id = str(fallback_id) if isinstance(fallback_id, str) else f"invalid_{_path_hash(manifest_path)}"
    safe_id = _safe_id(safe_id)
    return PluginManifest.model_construct(
        id=safe_id,
        name=str(fallback_name or safe_id),
        version=str(raw.get("version") or "0.0.0") if isinstance(raw, dict) else "0.0.0",
        api_version=str(raw.get("api_version") or "invalid") if isinstance(raw, dict) else "invalid",
        entrypoint=str(raw.get("entrypoint") or "plugin.py:register") if isinstance(raw, dict) else "plugin.py:register",
        type=PluginType.TOOL,
        capabilities=tuple(raw.get("capabilities") or ()) if isinstance(raw, dict) else (),
        permissions=tuple(raw.get("permissions") or ()) if isinstance(raw, dict) else (),
        activation=raw.get("activation") or {} if isinstance(raw, dict) else {},
        compatibility=CompatibilitySpec(),
        config_schema=raw.get("config_schema") or {} if isinstance(raw, dict) else {},
    )


def _mark_duplicate_ids(discovered: list[DiscoveredPlugin]) -> list[DiscoveredPlugin]:
    seen: dict[str, DiscoveredPlugin] = {}
    for item in discovered:
        existing = seen.get(item.manifest.id)
        if existing is None:
            seen[item.manifest.id] = item
            continue
        diagnostic = PluginDiagnostic(
            plugin_id=item.manifest.id,
            severity=PluginDiagnosticSeverity.ERROR,
            code="duplicate_plugin_id",
            message="Plugin id is discovered from more than one manifest.",
            path=str(item.manifest_path),
            details={"first_manifest_path": str(existing.manifest_path)},
        )
        item.diagnostics.append(diagnostic)
        existing.diagnostics.append(
            diagnostic.model_copy(update={"path": str(existing.manifest_path)})
        )
    return discovered


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _path_hash(path: Path) -> str:
    return hashlib.sha256(str(path).encode("utf-8", errors="replace")).hexdigest()[:12]


def _safe_id(value: str) -> str:
    safe = "".join(ch if ch.isalnum() or ch == "_" else "_" for ch in value.lower())
    if not safe or not safe[0].isalpha():
        safe = f"invalid_{safe}"
    return safe[:64]
