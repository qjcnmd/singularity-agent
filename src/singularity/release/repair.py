from __future__ import annotations

import shutil
import zipfile
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.release.doctor import run_doctor
from singularity.release.init import initialize_runtime
from singularity.release.migrations import load_manifest
from singularity.release.paths import RuntimePaths


PROTECTED_USER_DATA_DIRS = {"memory", "traces", "eval", "logs"}


def repair_runtime(paths: RuntimePaths) -> dict[str, Any]:
    before = run_doctor(paths).to_dict()
    init_result = initialize_runtime(paths, force=False)
    after = run_doctor(paths).to_dict()
    return {"before": before, "repair": init_result, "after": after}


def uninstall_plan(paths: RuntimePaths, *, purge_user_data: bool = False) -> dict[str, Any]:
    owned, reason = _runtime_is_owned(paths)
    if not owned:
        return {
            "root": str(paths.root),
            "purge_user_data": purge_user_data,
            "blocked": True,
            "reason": reason,
            "delete": [],
            "preserve": [],
        }
    managed = {
        "config": paths.config_dir,
        "state": paths.state_dir,
        "cache": paths.cache_dir,
        "logs": paths.logs_dir,
        "traces": paths.traces_dir,
        "memory": paths.memory_dir,
        "eval": paths.eval_dir,
        "backups": paths.backups_dir,
        "tmp": paths.tmp_dir,
    }
    delete: list[str] = []
    preserve: list[str] = []
    for name, path in managed.items():
        if not path.exists():
            continue
        if name in PROTECTED_USER_DATA_DIRS and not purge_user_data:
            preserve.append(str(path))
        else:
            delete.append(str(path))
    return {
        "root": str(paths.root),
        "purge_user_data": purge_user_data,
        "blocked": False,
        "delete": sorted(delete),
        "preserve": sorted(preserve),
    }


def uninstall_runtime(
    paths: RuntimePaths,
    *,
    dry_run: bool = True,
    purge_user_data: bool = False,
) -> dict[str, Any]:
    plan = uninstall_plan(paths, purge_user_data=purge_user_data)
    if plan.get("blocked"):
        plan["dry_run"] = dry_run
        return plan
    if dry_run:
        plan["dry_run"] = True
        return plan
    for raw_path in plan["delete"]:
        path = Path(raw_path)
        if path.is_dir():
            shutil.rmtree(path)
        elif path.exists():
            path.unlink()
    plan["dry_run"] = False
    return plan


def export_user_data(paths: RuntimePaths, output: Path | str) -> dict[str, Any]:
    output_path = Path(output).expanduser().resolve(strict=False)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    included_roots = [
        paths.config_dir,
        paths.state_dir,
        paths.logs_dir,
        paths.traces_dir,
        paths.memory_dir,
        paths.eval_dir,
        paths.backups_dir,
    ]
    manifest = {
        "schema_version": "singularity.export/v1",
        "created_at": datetime.now(UTC).isoformat(),
        "runtime_root": "redacted",
        "included": [],
    }
    with zipfile.ZipFile(output_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for root in included_roots:
            if not root.exists():
                continue
            manifest["included"].append(root.name)
            for path in root.rglob("*"):
                if path.is_file():
                    if path.resolve(strict=False) == output_path:
                        continue
                    archive.write(path, arcname=f"{root.name}/{path.relative_to(root).as_posix()}")
        archive.writestr("manifest.json", _json(manifest))
    return {"output": str(output_path), "included": manifest["included"]}


def _json(payload: dict[str, Any]) -> str:
    import json

    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def _runtime_is_owned(paths: RuntimePaths) -> tuple[bool, str | None]:
    if not paths.manifest_file.exists():
        return False, f"Singularity runtime manifest not found: {paths.manifest_file}"
    try:
        manifest = load_manifest(paths)
    except Exception as exc:
        return False, f"Singularity runtime manifest is unreadable: {type(exc).__name__}: {exc}"
    if not manifest.app_version:
        return False, "Singularity runtime manifest is missing app_version."
    return True, None
