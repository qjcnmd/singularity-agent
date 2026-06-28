from __future__ import annotations

import os
import sys

from singularity.release.init import load_config, validate_config
from singularity.release.metadata import optional_feature_status, requires_python, version_info
from singularity.release.migrations import load_manifest, pending_migrations
from singularity.release.models import (
    CONFIG_SCHEMA_VERSION,
    EVAL_SCHEMA_VERSION,
    MEMORY_SCHEMA_VERSION,
    TRACE_SCHEMA_VERSION,
    ReleaseCheck,
    ReleaseDoctorReport,
)
from singularity.release.paths import UserDataPaths


def run_doctor(paths: UserDataPaths) -> ReleaseDoctorReport:
    checks: list[ReleaseCheck] = []
    checks.append(_python_check())
    info = version_info(paths)
    checks.append(
        ReleaseCheck(
            "cli_installation",
            "ok" if info.installed_package else "warning",
            "CLI package metadata found." if info.installed_package else "CLI is running from a source checkout.",
            suggestion="Install with pipx or pip install . for production CLI use."
            if not info.installed_package
            else None,
            details={"install_path": info.install_path},
        )
    )
    checks.extend(_directory_checks(paths))
    checks.append(_config_check(paths))
    checks.append(_critical_component_config_check(paths))
    checks.append(_optional_dependencies_check())
    checks.append(_migration_check(paths))
    return ReleaseDoctorReport(
        ok=all(check.status != "error" for check in checks),
        checks=checks,
    )


def _python_check() -> ReleaseCheck:
    spec = requires_python()
    ok = _supports_python(spec)
    return ReleaseCheck(
        "python_version",
        "ok" if ok else "error",
        f"Python {sys.version.split()[0]} {'satisfies' if ok else 'does not satisfy'} {spec}.",
        suggestion=f"Use Python matching {spec}." if not ok else None,
        details={"requires_python": spec},
    )


def _directory_checks(paths: UserDataPaths) -> list[ReleaseCheck]:
    checks: list[ReleaseCheck] = []
    for name, path in (
        ("user_data_root", paths.root),
        ("config_dir", paths.config_dir),
        ("state_dir", paths.state_dir),
        ("cache_dir", paths.cache_dir),
        ("logs_dir", paths.logs_dir),
        ("traces_dir", paths.traces_dir),
        ("memory_dir", paths.memory_dir),
        ("eval_dir", paths.eval_dir),
        ("backups_dir", paths.backups_dir),
        ("tmp_dir", paths.tmp_dir),
    ):
        if not path.exists():
            checks.append(
                ReleaseCheck(
                    name,
                    "error",
                    f"{path} does not exist.",
                    suggestion="Run singularity-agent system init or singularity-agent system repair.",
                )
            )
        elif not path.is_dir():
            checks.append(ReleaseCheck(name, "error", f"{path} is not a directory."))
        elif not os.access(path, os.W_OK | os.R_OK):
            checks.append(
                ReleaseCheck(
                    name,
                    "error",
                    f"{path} is not readable and writable.",
                    suggestion="Fix filesystem permissions, then rerun doctor.",
                )
            )
        else:
            checks.append(ReleaseCheck(name, "ok", f"{path} is accessible."))
    return checks


def _config_check(paths: UserDataPaths) -> ReleaseCheck:
    if not paths.config_file.exists():
        return ReleaseCheck(
            "config_schema",
            "error",
            f"{paths.config_file} does not exist.",
            suggestion="Run singularity-agent system init or singularity-agent system repair.",
        )
    try:
        issues = validate_config(load_config(paths))
    except Exception as exc:
        return ReleaseCheck(
            "config_schema",
            "error",
            f"Config is unreadable: {type(exc).__name__}: {exc}",
            suggestion="Run singularity-agent system repair or restore from backup.",
        )
    return ReleaseCheck(
        "config_schema",
        "error" if issues else "ok",
        "Config schema is valid." if not issues else "; ".join(issues),
        suggestion=f"Expected schema_version {CONFIG_SCHEMA_VERSION}."
        if issues
        else None,
    )


def _critical_component_config_check(paths: UserDataPaths) -> ReleaseCheck:
    try:
        config = load_config(paths)
    except Exception:
        return ReleaseCheck(
            "component_configuration",
            "error",
            "Installation configuration is unavailable.",
            suggestion="Run singularity-agent system init or singularity-agent system repair.",
        )
    missing_sections = [
        section
        for section in ("model", "provider")
        if not isinstance(config.get(section), dict)
    ]
    missing_env = [
        name
        for name in ("SINGULARITY_API_KEY", "SINGULARITY_BASE_URL", "SINGULARITY_MODEL")
        if not os.getenv(name)
    ]
    if missing_sections:
        return ReleaseCheck(
            "component_configuration",
            "error",
            "Missing component configuration sections: " + ", ".join(missing_sections),
            suggestion="Run singularity-agent system repair.",
        )
    if missing_env:
        return ReleaseCheck(
            "component_configuration",
            "warning",
            "Model/provider environment is incomplete: " + ", ".join(missing_env),
            suggestion="Set these variables before running model-backed commands.",
        )
    return ReleaseCheck("component_configuration", "ok", "Model/provider sections are configured.")


def _optional_dependencies_check() -> ReleaseCheck:
    status = optional_feature_status()
    missing = {
        feature: payload["missing"]
        for feature, payload in status.items()
        if payload["missing"]
    }
    return ReleaseCheck(
        "optional_dependencies",
        "warning" if missing else "ok",
        "Optional dependencies are available." if not missing else "Some optional dependencies are missing.",
        suggestion="Install the matching extras only if you need those features." if missing else None,
        details=status,
    )


def _migration_check(paths: UserDataPaths) -> ReleaseCheck:
    if not paths.manifest_file.exists():
        return ReleaseCheck(
            "migrations",
            "error",
            f"{paths.manifest_file} does not exist.",
            suggestion="Run singularity-agent system init.",
        )
    try:
        manifest = load_manifest(paths)
        pending = pending_migrations(paths)
    except Exception as exc:
        return ReleaseCheck(
            "migrations",
            "error",
            f"Migration state is unreadable: {type(exc).__name__}: {exc}",
            suggestion="Run singularity-agent system repair or restore from backup.",
        )
    schema_ok = (
        manifest.config_schema_version == CONFIG_SCHEMA_VERSION
        and manifest.memory_schema_version == MEMORY_SCHEMA_VERSION
        and manifest.trace_schema_version == TRACE_SCHEMA_VERSION
        and manifest.eval_schema_version == EVAL_SCHEMA_VERSION
    )
    if not schema_ok:
        return ReleaseCheck(
            "migrations",
            "error",
            "Manifest schema versions are unsupported.",
            suggestion="Run singularity-agent system migrate after backing up user data.",
            details=manifest.to_dict(),
        )
    if pending:
        return ReleaseCheck(
            "migrations",
            "warning",
            "Pending migrations: " + ", ".join(migration.version for migration in pending),
            suggestion="Run singularity-agent system migrate.",
        )
    return ReleaseCheck("migrations", "ok", "No pending migrations.")


def _supports_python(spec: str) -> bool:
    spec = spec.strip()
    if spec.startswith(">="):
        version = tuple(int(part) for part in spec[2:].split(".")[:2])
        return sys.version_info[:2] >= version
    return True
