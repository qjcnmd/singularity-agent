from __future__ import annotations

import shutil
from collections.abc import Callable
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from singularity.release.init import default_config, validate_config
from singularity.release.metadata import package_version
from singularity.release.models import (
    CURRENT_MIGRATION_VERSION,
    InstallationManifest,
    atomic_write_json,
    read_json,
)
from singularity.release.paths import UserDataPaths
from singularity.utils.serialization import utc_iso_timestamp

MigrationFn = Callable[[UserDataPaths], None]


@dataclass(frozen=True)
class Migration:
    version: str
    name: str
    apply: MigrationFn


def pending_migrations(
    paths: UserDataPaths,
    *,
    migrations: list[Migration] | None = None,
) -> list[Migration]:
    manifest = load_manifest(paths)
    last = manifest.last_migration
    return [migration for migration in (migrations or MIGRATIONS) if migration.version > last]


def apply_migrations(
    paths: UserDataPaths,
    *,
    migrations: list[Migration] | None = None,
) -> list[dict[str, str]]:
    applied: list[dict[str, str]] = []
    for migration in pending_migrations(paths, migrations=migrations):
        backup = backup_installation(paths, label=f"migration-{migration.version}")
        try:
            migration.apply(paths)
            manifest = load_manifest(paths)
            now = _now()
            atomic_write_json(
                paths.manifest_file,
                InstallationManifest(
                    app_version=package_version(),
                    config_schema_version=manifest.config_schema_version,
                    memory_schema_version=manifest.memory_schema_version,
                    trace_schema_version=manifest.trace_schema_version,
                    eval_schema_version=manifest.eval_schema_version,
                    last_migration=migration.version,
                    mode=paths.mode.value,
                    created_at=manifest.created_at,
                    updated_at=now,
                ).to_dict(),
            )
            applied.append({"version": migration.version, "name": migration.name, "backup": str(backup)})
        except Exception:
            restore_installation_backup(paths, backup)
            raise
    return applied


def load_manifest(paths: UserDataPaths) -> InstallationManifest:
    if not paths.manifest_file.exists():
        raise FileNotFoundError(paths.manifest_file)
    return InstallationManifest.from_dict(read_json(paths.manifest_file))


def backup_installation(paths: UserDataPaths, *, label: str) -> Path:
    target = paths.backups_dir / f"{label}-{datetime.now(UTC).strftime('%Y%m%dT%H%M%SZ')}"
    target.mkdir(parents=True, exist_ok=False)
    for name, source in (("config", paths.config_dir), ("state", paths.state_dir)):
        if source.exists():
            shutil.copytree(source, target / name)
    return target


def restore_installation_backup(paths: UserDataPaths, backup: Path) -> None:
    for name, target in (("config", paths.config_dir), ("state", paths.state_dir)):
        saved = backup / name
        if target.exists():
            shutil.rmtree(target)
        if saved.exists():
            shutil.copytree(saved, target)


def _installation_layout_v1(paths: UserDataPaths) -> None:
    if not paths.config_file.exists():
        atomic_write_json(paths.config_file, default_config(paths))
        return
    config = read_json(paths.config_file)
    if validate_config(config):
        return
    config.setdefault("component", {})["mode"] = paths.mode.value
    config["component"]["root"] = str(paths.root)
    atomic_write_json(paths.config_file, config)


MIGRATIONS = [
    Migration(CURRENT_MIGRATION_VERSION, "release installation manifest and config paths", _installation_layout_v1),
]


_now = utc_iso_timestamp
