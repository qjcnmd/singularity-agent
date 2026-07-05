from __future__ import annotations

from typing import Any

from singularity.release.metadata import package_version
from singularity.release.models import (
    CONFIG_SCHEMA_VERSION,
    CURRENT_MIGRATION_VERSION,
    InstallationManifest,
    atomic_write_json,
    read_json,
)
from singularity.release.paths import UserDataPaths
from singularity.utils.serialization import utc_iso_timestamp


def initialize_user_data(paths: UserDataPaths, *, force: bool = False) -> dict[str, Any]:
    created_dirs: list[str] = []
    for directory in paths.directories():
        if not directory.exists():
            created_dirs.append(str(directory))
        directory.mkdir(parents=True, exist_ok=True)

    wrote_config = False
    if force or not paths.config_file.exists():
        atomic_write_json(paths.config_file, default_config(paths))
        wrote_config = True

    wrote_manifest = False
    if force or not paths.manifest_file.exists():
        now = _now()
        atomic_write_json(
            paths.manifest_file,
            InstallationManifest(
                app_version=package_version(),
                last_migration=CURRENT_MIGRATION_VERSION,
                mode=paths.mode.value,
                created_at=now,
                updated_at=now,
            ).to_dict(),
        )
        wrote_manifest = True

    return {
        "root": str(paths.root),
        "created_dirs": created_dirs,
        "config": str(paths.config_file),
        "config_written": wrote_config,
        "manifest": str(paths.manifest_file),
        "manifest_written": wrote_manifest,
    }


def default_config(paths: UserDataPaths) -> dict[str, Any]:
    return {
        "schema_version": CONFIG_SCHEMA_VERSION,
        "component": {
            "mode": paths.mode.value,
            "root": str(paths.root),
        },
        "model": {
            "provider": "openai_compatible",
            "model_env": "SINGULARITY_MODEL",
        },
        "provider": {
            "base_url_env": "SINGULARITY_BASE_URL",
            "api_key_env": "SINGULARITY_API_KEY",
        },
    }


def load_config(paths: UserDataPaths) -> dict[str, Any]:
    return read_json(paths.config_file)


def validate_config(payload: dict[str, Any]) -> list[str]:
    issues: list[str] = []
    if payload.get("schema_version") != CONFIG_SCHEMA_VERSION:
        issues.append("unsupported config schema_version")
    for key in ("component", "model", "provider"):
        if not isinstance(payload.get(key), dict):
            issues.append(f"missing {key} section")
    return issues


_now = utc_iso_timestamp
