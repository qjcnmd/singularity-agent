from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from singularity.plugins.compatibility import compatibility_status
from singularity.plugins.models import (
    DiscoveredPlugin,
    PluginLockEntry,
    PluginStatus,
)


class PluginStatusStore:
    def __init__(self, project_root: Path | str) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.path = self.project_root / ".singularity" / "plugin-status.json"

    def load(self) -> dict[str, PluginStatus]:
        payload = self._load_payload()
        plugins = payload.get("plugins") if isinstance(payload, dict) else {}
        if not isinstance(plugins, dict):
            return {}
        statuses: dict[str, PluginStatus] = {}
        for plugin_id, value in plugins.items():
            if isinstance(value, dict):
                try:
                    statuses[plugin_id] = PluginStatus.model_validate(value)
                except Exception:
                    continue
        return statuses

    def get(self, plugin_id: str) -> PluginStatus | None:
        return self.load().get(plugin_id)

    def enable(
        self,
        plugin: DiscoveredPlugin,
        *,
        config: dict[str, Any] | None = None,
        compatibility_diagnostics: list[Any] | None = None,
    ) -> PluginStatus:
        statuses = self.load()
        status = PluginStatus(
            enabled=True,
            version=plugin.manifest.version,
            path=str(plugin.plugin_dir),
            manifest_hash=plugin.manifest_hash,
            approved_permissions=tuple(plugin.manifest.permissions),
            config=config or {},
            compatibility_status=compatibility_status(compatibility_diagnostics or []),
        )
        statuses[plugin.manifest.id] = status
        self._save(statuses)
        return status

    def disable(self, plugin_id: str) -> PluginStatus:
        statuses = self.load()
        status = statuses.get(plugin_id) or PluginStatus()
        status = status.model_copy(update={"enabled": False})
        statuses[plugin_id] = status
        self._save(statuses)
        return status

    def enabled_for(self, plugin: DiscoveredPlugin) -> PluginStatus | None:
        status = self.get(plugin.manifest.id)
        if status is None or not status.enabled:
            return None
        if status.path and Path(status.path).resolve(strict=False) != plugin.plugin_dir.resolve(strict=False):
            return None
        if status.manifest_hash and status.manifest_hash != plugin.manifest_hash:
            return None
        return status

    def _load_payload(self) -> dict[str, Any]:
        if not self.path.exists():
            return {"plugins": {}}
        try:
            payload = json.loads(self.path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            return {"plugins": {}}
        return payload if isinstance(payload, dict) else {"plugins": {}}

    def _save(self, statuses: dict[str, PluginStatus]) -> None:
        payload = {
            "version": 1,
            "plugins": {
                plugin_id: status.model_dump(mode="json")
                for plugin_id, status in sorted(statuses.items())
            },
        }
        _atomic_write_json(self.path, payload)


class PluginLockStore:
    def __init__(self, project_root: Path | str) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.path = self.project_root / ".singularity" / "plugin-lock.json"

    def write_entries(self, entries: list[PluginLockEntry]) -> None:
        payload = {
            "version": 1,
            "plugins": [entry.model_dump(mode="json") for entry in entries],
        }
        _atomic_write_json(self.path, payload)

    def load(self) -> list[PluginLockEntry]:
        if not self.path.exists():
            return []
        try:
            payload = json.loads(self.path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            return []
        plugins = payload.get("plugins") if isinstance(payload, dict) else []
        if not isinstance(plugins, list):
            return []
        entries: list[PluginLockEntry] = []
        for item in plugins:
            if not isinstance(item, dict):
                continue
            try:
                entries.append(PluginLockEntry.model_validate(item))
            except Exception:
                continue
        return entries


def _atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)
