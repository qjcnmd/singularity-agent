from __future__ import annotations

import importlib.metadata
import importlib.util
import json
import os
import sqlite3
import sys
from collections.abc import Iterable
from pathlib import Path
from typing import Any

from singularity.code_index.models import SCHEMA_VERSION as INDEX_SCHEMA_VERSION
from singularity.diagnostics.models import (
    DiagnosticCheck,
    DiagnosticContext,
    DiagnosticFinding,
    DiagnosticSeverity,
)
from singularity.evaluation.store import TASK_SET_SCHEMA_VERSION
from singularity.memory.models import SCHEMA_VERSION as MEMORY_ENTRY_SCHEMA_VERSION
from singularity.release.init import load_config, validate_config
from singularity.release.metadata import optional_feature_status, requires_python, version_info
from singularity.release.migrations import load_manifest, pending_migrations
from singularity.release.models import (
    CONFIG_SCHEMA_VERSION,
    EVAL_SCHEMA_VERSION,
    MEMORY_SCHEMA_VERSION,
    TRACE_SCHEMA_VERSION,
)


def default_checks() -> list[DiagnosticCheck]:
    return [
        DiagnosticCheck("environment.python", "environment", DiagnosticSeverity.INFO, _python_check),
        DiagnosticCheck("environment.package", "environment", DiagnosticSeverity.INFO, _package_check),
        DiagnosticCheck(
            "environment.optional_dependencies",
            "environment",
            DiagnosticSeverity.WARNING,
            _optional_dependencies_check,
        ),
        DiagnosticCheck("environment.entry_point", "environment", DiagnosticSeverity.INFO, _entry_point_check),
        DiagnosticCheck("environment.virtualenv", "environment", DiagnosticSeverity.INFO, _virtualenv_check),
        DiagnosticCheck("config.file", "config", DiagnosticSeverity.ERROR, _config_file_check),
        DiagnosticCheck("config.provider", "config", DiagnosticSeverity.WARNING, _provider_check),
        DiagnosticCheck("filesystem.user_data_dirs", "filesystem", DiagnosticSeverity.ERROR, _user_data_dirs_check),
        DiagnosticCheck("filesystem.workspace_dirs", "filesystem", DiagnosticSeverity.SUGGESTION, _workspace_dirs_check),
        DiagnosticCheck("component.graph", "component", DiagnosticSeverity.INFO, _component_graph_check),
        DiagnosticCheck("schema.migrations", "schema", DiagnosticSeverity.WARNING, _migration_check),
        DiagnosticCheck("schema.memory_index", "schema", DiagnosticSeverity.SUGGESTION, _memory_index_check),
        DiagnosticCheck("schema.project_index", "schema", DiagnosticSeverity.SUGGESTION, _project_index_check),
        DiagnosticCheck("data_integrity.json_payloads", "data-integrity", DiagnosticSeverity.ERROR, _json_payloads_check),
        DiagnosticCheck("data_integrity.trace_indexes", "data-integrity", DiagnosticSeverity.WARNING, _trace_indexes_check),
    ]


def _finding(
    context: DiagnosticContext,
    check_id: str,
    group: str,
    severity: DiagnosticSeverity,
    status: str,
    message: str,
    technical_detail: str,
    suggested_fix: str,
    *,
    auto_repairable: bool = False,
    details: dict[str, Any] | None = None,
) -> DiagnosticFinding:
    return DiagnosticFinding(
        check_id=check_id,
        group=group,
        severity=severity,
        status=status,
        message=message,
        technical_detail=technical_detail,
        suggested_fix=suggested_fix,
        auto_repairable=auto_repairable,
        details=details or {},
    )


def _python_check(context: DiagnosticContext) -> DiagnosticFinding:
    spec = requires_python()
    ok = _supports_python(spec)
    return _finding(
        context,
        "environment.python",
        "environment",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.ERROR,
        "passed" if ok else "failed",
        f"Python {sys.version.split()[0]} {'satisfies' if ok else 'does not satisfy'} {spec}.",
        f"requires_python={spec}; executable={sys.executable}",
        f"Use Python matching {spec}." if not ok else "No action needed.",
    )


def _package_check(context: DiagnosticContext) -> DiagnosticFinding:
    info = version_info(context.paths)
    severity = DiagnosticSeverity.INFO if info.installed_package else DiagnosticSeverity.WARNING
    return _finding(
        context,
        "environment.package",
        "environment",
        severity,
        "passed" if info.installed_package else "failed",
        "Singularity package metadata is available." if info.installed_package else "Singularity is running from a source checkout.",
        f"version={info.version}; install_path={info.install_path}; installed_package={info.installed_package}",
        "Install with pipx or pip install . for production CLI use." if not info.installed_package else "No action needed.",
        details=info.to_dict(),
    )


def _entry_point_check(context: DiagnosticContext) -> DiagnosticFinding:
    matches: dict[str, list[str]] = {"singularity-agent": [], "sg": []}
    try:
        entry_points = importlib.metadata.entry_points()
        for name in matches:
            selected = entry_points.select(group="console_scripts", name=name)
            matches[name] = [str(item.value) for item in selected]
    except Exception:
        matches = {"singularity-agent": [], "sg": []}
    ok = all("singularity.cli:main" in values for values in matches.values())
    detail = "; ".join(
        f"console_scripts.{name}=" + (", ".join(values) if values else "<missing>")
        for name, values in matches.items()
    )
    return _finding(
        context,
        "environment.entry_point",
        "environment",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.WARNING,
        "passed" if ok else "failed",
        "Console entry points target singularity.cli:main." if ok else "Console entry point metadata is unavailable.",
        detail,
        "Install the package in editable or user mode if command discovery is required." if not ok else "No action needed.",
        details={"entry_points": matches},
    )


def _optional_dependencies_check(context: DiagnosticContext) -> DiagnosticFinding:
    status = optional_feature_status()
    missing = {
        feature: payload["missing"]
        for feature, payload in status.items()
        if payload["missing"]
    }
    ok = not missing
    return _finding(
        context,
        "environment.optional_dependencies",
        "environment",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.WARNING,
        "passed" if ok else "failed",
        "Optional dependencies are available." if ok else "Some optional dependencies are missing.",
        "missing=" + (json.dumps(missing, ensure_ascii=False, sort_keys=True) if missing else "<none>"),
        "Install matching extras only for features you use." if missing else "No action needed.",
        details=status,
    )


def _virtualenv_check(context: DiagnosticContext) -> DiagnosticFinding:
    in_venv = sys.prefix != getattr(sys, "base_prefix", sys.prefix) or bool(os.getenv("VIRTUAL_ENV"))
    return _finding(
        context,
        "environment.virtualenv",
        "environment",
        DiagnosticSeverity.INFO if in_venv else DiagnosticSeverity.SUGGESTION,
        "passed" if in_venv else "failed",
        "Python is running inside a virtual environment." if in_venv else "Python is not running inside an activated virtual environment.",
        f"prefix={sys.prefix}; base_prefix={getattr(sys, 'base_prefix', '')}; VIRTUAL_ENV={'set' if os.getenv('VIRTUAL_ENV') else 'unset'}",
        "Use the project virtual environment for reproducible local runs." if not in_venv else "No action needed.",
    )


def _config_file_check(context: DiagnosticContext) -> DiagnosticFinding:
    if not context.paths.config_file.exists():
        return _finding(
            context,
            "config.file",
            "config",
            DiagnosticSeverity.ERROR,
            "failed",
            "Singularity config file is missing.",
            f"{context.paths.config_file} does not exist.",
            "Run singularity-agent repair --apply or singularity-agent system init.",
            auto_repairable=True,
            details={"repair": "write_default_config", "path": str(context.paths.config_file)},
        )
    try:
        payload = load_config(context.paths)
    except Exception as exc:
        return _finding(
            context,
            "config.file",
            "config",
            DiagnosticSeverity.ERROR,
            "failed",
            "Singularity config file is unreadable.",
            f"{type(exc).__name__}: {exc}",
            "Fix the JSON manually or restore from backup. Automatic repair will not guess user config.",
            details={"path": str(context.paths.config_file), "error_type": type(exc).__name__},
        )
    issues = validate_config(payload)
    if issues:
        return _finding(
            context,
            "config.file",
            "config",
            DiagnosticSeverity.ERROR,
            "failed",
            "Singularity config schema is incomplete or unsupported.",
            "; ".join(issues),
            f"Merge missing defaults for schema_version {CONFIG_SCHEMA_VERSION}.",
            auto_repairable=True,
            details={"repair": "merge_default_config", "issues": issues, "path": str(context.paths.config_file)},
        )
    return _finding(
        context,
        "config.file",
        "config",
        DiagnosticSeverity.INFO,
        "passed",
        "Singularity config schema is valid.",
        f"{context.paths.config_file} schema_version={payload.get('schema_version')}",
        "No action needed.",
        details={"path": str(context.paths.config_file), "schema_version": payload.get("schema_version")},
    )


def _provider_check(context: DiagnosticContext) -> DiagnosticFinding:
    try:
        payload = load_config(context.paths)
    except Exception as exc:
        return _finding(
            context,
            "config.provider",
            "config",
            DiagnosticSeverity.WARNING,
            "failed",
            "Provider config cannot be checked because config is unavailable.",
            f"{type(exc).__name__}: {exc}",
            "Fix config.file first.",
        )
    provider = payload.get("provider") if isinstance(payload.get("provider"), dict) else {}
    model = payload.get("model") if isinstance(payload.get("model"), dict) else {}
    refs = {
        "api_key_env": provider.get("api_key_env"),
        "base_url_env": provider.get("base_url_env"),
        "model_env": model.get("model_env"),
    }
    missing_refs = [name for name, value in refs.items() if not value]
    missing_env = [value for value in refs.values() if value and not os.getenv(str(value))]
    if missing_refs:
        return _finding(
            context,
            "config.provider",
            "config",
            DiagnosticSeverity.ERROR,
            "failed",
            "Provider config is missing required environment variable references.",
            "missing references: " + ", ".join([*missing_refs, "SINGULARITY_API_KEY"]),
            "Run singularity-agent repair --apply to merge default provider/model fields.",
            auto_repairable=True,
            details={"repair": "merge_default_config", "missing_refs": missing_refs, "refs": refs},
        )
    if missing_env:
        return _finding(
            context,
            "config.provider",
            "config",
            DiagnosticSeverity.WARNING,
            "failed",
            "Provider environment variables are not fully set.",
            "missing env vars: " + ", ".join(str(item) for item in missing_env),
            "Set these variables before running model-backed commands.",
            details={"refs": refs, "missing_env": missing_env},
        )
    return _finding(
        context,
        "config.provider",
        "config",
        DiagnosticSeverity.INFO,
        "passed",
        "Provider environment references are configured.",
        "configured env refs: " + ", ".join(str(item) for item in refs.values()),
        "No action needed.",
        details={"refs": refs},
    )


def _user_data_dirs_check(context: DiagnosticContext) -> DiagnosticFinding:
    managed = [context.paths.root, *context.paths.directories()]
    missing: list[str] = []
    not_dirs: list[str] = []
    unwritable: list[str] = []
    for path in managed:
        if not path.exists():
            missing.append(str(path))
        elif not path.is_dir():
            not_dirs.append(str(path))
        elif not os.access(path, os.R_OK | os.W_OK):
            unwritable.append(str(path))
    if missing or not_dirs or unwritable:
        repairable = bool(missing) and not not_dirs and not unwritable
        return _finding(
            context,
            "filesystem.user_data_dirs",
            "filesystem",
            DiagnosticSeverity.ERROR,
            "failed",
            "One or more user data directories are missing or inaccessible.",
            f"missing={missing}; not_dirs={not_dirs}; unwritable={unwritable}",
        "Run singularity-agent repair --apply to create missing directories." if repairable else "Fix path types or permissions manually.",
            auto_repairable=repairable,
            details={
                "repair": "create_dirs" if repairable else None,
                "missing": missing,
                "not_dirs": not_dirs,
                "unwritable": unwritable,
                "paths": [str(path) for path in managed],
            },
        )
    return _finding(
        context,
        "filesystem.user_data_dirs",
        "filesystem",
        DiagnosticSeverity.INFO,
        "passed",
        "User data directories are present and writable.",
        "; ".join(str(path) for path in managed),
        "No action needed.",
        details={"paths": [str(path) for path in managed]},
    )


def _workspace_dirs_check(context: DiagnosticContext) -> DiagnosticFinding:
    root = context.project_root / ".singularity"
    expected = [root, root / "memory" / "auto", root / "memory" / "human", root / "rules"]
    missing = [str(path) for path in expected if not path.exists()]
    if missing:
        return _finding(
            context,
            "filesystem.workspace_dirs",
            "filesystem",
            DiagnosticSeverity.SUGGESTION,
            "failed",
            "Workspace-local Singularity directories are not initialized.",
            "missing=" + ", ".join(missing),
            "They will be created lazily, or by repair when local memory/index repair is requested.",
            auto_repairable=True,
            details={"repair": "create_dirs", "missing": missing, "paths": [str(path) for path in expected]},
        )
    return _finding(
        context,
        "filesystem.workspace_dirs",
        "filesystem",
        DiagnosticSeverity.INFO,
        "passed",
        "Workspace-local Singularity directories exist.",
        "; ".join(str(path) for path in expected),
        "No action needed.",
        details={"paths": [str(path) for path in expected]},
    )


def _component_graph_check(context: DiagnosticContext) -> DiagnosticFinding:
    modules = {
        "tool_executor": "singularity.tools.executor",
        "command_executor": "singularity.command.executor",
        "sandbox_manager": "singularity.sandbox.manager",
        "policy_engine": "singularity.policy.engine",
        "approval_gate": "singularity.policy.approval",
    }
    missing = [name for name, module in modules.items() if importlib.util.find_spec(module) is None]
    ok = not missing
    return _finding(
        context,
        "component.graph",
        "component",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.ERROR,
        "passed" if ok else "failed",
        "Core execution component modules are importable." if ok else "Some core execution component modules are missing.",
        "missing=" + (", ".join(missing) if missing else "<none>"),
        "Reinstall Singularity or restore the missing package files." if missing else "No action needed.",
        details={"modules": modules, "missing": missing},
    )


def _migration_check(context: DiagnosticContext) -> DiagnosticFinding:
    if not context.paths.manifest_file.exists():
        return _finding(
            context,
            "schema.migrations",
            "schema",
            DiagnosticSeverity.ERROR,
            "failed",
            "Installation manifest is missing.",
            f"{context.paths.manifest_file} does not exist.",
            "Run singularity-agent repair --apply to create the manifest.",
            auto_repairable=True,
            details={"repair": "write_manifest", "path": str(context.paths.manifest_file)},
        )
    try:
        manifest = load_manifest(context.paths)
        pending = pending_migrations(context.paths)
    except Exception as exc:
        return _finding(
            context,
            "schema.migrations",
            "schema",
            DiagnosticSeverity.ERROR,
            "failed",
            "Installation manifest or migration state is unreadable.",
            f"{type(exc).__name__}: {exc}",
            "Restore the manifest from backup or repair manually.",
            details={"path": str(context.paths.manifest_file), "error_type": type(exc).__name__},
        )
    schema_ok = (
        manifest.config_schema_version == CONFIG_SCHEMA_VERSION
        and manifest.memory_schema_version == MEMORY_SCHEMA_VERSION
        and manifest.trace_schema_version == TRACE_SCHEMA_VERSION
        and manifest.eval_schema_version == EVAL_SCHEMA_VERSION
    )
    if not schema_ok:
        return _finding(
            context,
            "schema.migrations",
            "schema",
            DiagnosticSeverity.ERROR,
            "failed",
            "Installation manifest schema versions are unsupported.",
            json.dumps(manifest.to_dict(), ensure_ascii=False, sort_keys=True),
            "Back up user data, then run a supported migration or restore a compatible manifest.",
            details={"manifest": manifest.to_dict()},
        )
    if pending:
        return _finding(
            context,
            "schema.migrations",
            "schema",
            DiagnosticSeverity.WARNING,
            "failed",
            "Installation migrations are pending.",
            "pending=" + ", ".join(item.version for item in pending),
            "Run singularity-agent repair --apply to apply migrations with backup.",
            auto_repairable=True,
            details={"repair": "apply_migrations", "pending": [item.version for item in pending]},
        )
    return _finding(
        context,
        "schema.migrations",
        "schema",
        DiagnosticSeverity.INFO,
        "passed",
        "Installation manifest schema and migrations are current.",
        json.dumps(manifest.to_dict(), ensure_ascii=False, sort_keys=True),
        "No action needed.",
        details={"manifest": manifest.to_dict()},
    )


def _memory_index_check(context: DiagnosticContext) -> DiagnosticFinding:
    index = context.project_root / ".singularity" / "memory" / "auto" / "index.json"
    if not index.exists():
        return _finding(
            context,
            "schema.memory_index",
            "schema",
            DiagnosticSeverity.SUGGESTION,
            "failed",
            "Workspace memory index is missing.",
            f"{index} does not exist.",
            "Run singularity-agent repair --apply to rebuild the derived memory index.",
            auto_repairable=True,
            details={"repair": "rebuild_memory_index", "path": str(index)},
        )
    try:
        payload = json.loads(index.read_text(encoding="utf-8"))
    except Exception as exc:
        return _finding(
            context,
            "schema.memory_index",
            "schema",
            DiagnosticSeverity.WARNING,
            "failed",
            "Workspace memory index is unreadable.",
            f"{type(exc).__name__}: {exc}",
            "Run singularity-agent repair --apply to rebuild the derived memory index.",
            auto_repairable=True,
            details={"repair": "rebuild_memory_index", "path": str(index)},
        )
    ok = payload.get("schema_version") == MEMORY_ENTRY_SCHEMA_VERSION
    return _finding(
        context,
        "schema.memory_index",
        "schema",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.WARNING,
        "passed" if ok else "failed",
        "Workspace memory index schema is current." if ok else "Workspace memory index schema is unsupported.",
        f"schema_version={payload.get('schema_version')}; expected={MEMORY_ENTRY_SCHEMA_VERSION}",
        "No action needed." if ok else "Run singularity-agent repair --apply to rebuild the derived memory index.",
        auto_repairable=not ok,
        details={"repair": None if ok else "rebuild_memory_index", "path": str(index)},
    )


def _project_index_check(context: DiagnosticContext) -> DiagnosticFinding:
    db_path = context.project_root / ".singularity" / "index.sqlite"
    if not db_path.exists():
        return _finding(
            context,
            "schema.project_index",
            "schema",
            DiagnosticSeverity.SUGGESTION,
            "failed",
            "Project index database is missing.",
            f"{db_path} does not exist.",
            "Run singularity-agent repair --apply to rebuild the derived project index.",
            auto_repairable=True,
            details={"repair": "rebuild_project_index", "path": str(db_path)},
        )
    try:
        with sqlite3.connect(f"file:{db_path}?mode=ro", uri=True) as db:
            row = db.execute("select value from index_metadata where key = 'schema_version'").fetchone()
        schema_version = row[0] if row else None
    except Exception as exc:
        return _finding(
            context,
            "schema.project_index",
            "schema",
            DiagnosticSeverity.WARNING,
            "failed",
            "Project index database is unreadable.",
            f"{type(exc).__name__}: {exc}",
            "Run singularity-agent repair --apply to rebuild the derived project index.",
            auto_repairable=True,
            details={"repair": "rebuild_project_index", "path": str(db_path)},
        )
    ok = schema_version == INDEX_SCHEMA_VERSION
    return _finding(
        context,
        "schema.project_index",
        "schema",
        DiagnosticSeverity.INFO if ok else DiagnosticSeverity.WARNING,
        "passed" if ok else "failed",
        "Project index schema is current." if ok else "Project index schema is unsupported.",
        f"schema_version={schema_version}; expected={INDEX_SCHEMA_VERSION}",
        "No action needed." if ok else "Run singularity-agent repair --apply to rebuild the derived project index.",
        auto_repairable=not ok,
        details={"repair": None if ok else "rebuild_project_index", "path": str(db_path)},
    )


def _json_payloads_check(context: DiagnosticContext) -> DiagnosticFinding:
    issues: list[dict[str, str]] = []
    for path in _json_files(context):
        _check_json_file(path, issues)
    for path in _jsonl_files(context):
        _check_jsonl_file(path, issues)
    if issues:
        detail = "; ".join(f"{item['path']}: {item['error']}" for item in issues[:20])
        return _finding(
            context,
            "data_integrity.json_payloads",
            "data-integrity",
            DiagnosticSeverity.ERROR,
            "failed",
            "Some local JSON/JSONL payloads are unreadable or structurally invalid.",
            detail,
            "Repair manually or restore from backup. Automatic repair will not rewrite original user data.",
            details={"issues": issues[:100]},
        )
    return _finding(
        context,
        "data_integrity.json_payloads",
        "data-integrity",
        DiagnosticSeverity.INFO,
        "passed",
        "Local JSON/JSONL payloads checked by doctor are readable.",
        "No unreadable config, manifest, memory, trace, or eval payloads were found in the bounded scan.",
        "No action needed.",
    )


def _trace_indexes_check(context: DiagnosticContext) -> DiagnosticFinding:
    if not context.paths.traces_dir.exists():
        return _finding(
            context,
            "data_integrity.trace_indexes",
            "data-integrity",
            DiagnosticSeverity.INFO,
            "passed",
            "Trace directory does not exist yet.",
            f"{context.paths.traces_dir} is absent.",
            "No action needed.",
        )
    missing_indexes: list[str] = []
    orphan_indexes: list[str] = []
    invalid_payloads: list[str] = []
    for run_dir in context.paths.traces_dir.iterdir():
        if not run_dir.is_dir():
            continue
        payload_paths = [run_dir / name for name in ("events.jsonl", "spans.jsonl", "artifacts.jsonl")]
        existing_payloads = [path for path in payload_paths if path.exists()]
        for path in existing_payloads:
            if not _jsonl_valid(path):
                invalid_payloads.append(str(path))
        has_trace_payload = bool(existing_payloads)
        index = run_dir / "index.json"
        if has_trace_payload and not index.exists():
            missing_indexes.append(str(index))
        elif index.exists() and not has_trace_payload:
            orphan_indexes.append(str(index))
    if missing_indexes or orphan_indexes or invalid_payloads:
        repairable = bool(missing_indexes) and not orphan_indexes and not invalid_payloads
        return _finding(
            context,
            "data_integrity.trace_indexes",
            "data-integrity",
            DiagnosticSeverity.WARNING,
            "failed",
            "Trace index files are missing or orphaned.",
            "missing_indexes="
            + ", ".join(missing_indexes)
            + "; orphan_indexes="
            + ", ".join(orphan_indexes)
            + "; invalid_payloads="
            + ", ".join(invalid_payloads),
            "Run singularity-agent repair --apply to rebuild missing derived trace indexes."
            if repairable
            else "Inspect trace payloads or orphan trace indexes manually; automatic repair will not delete or rewrite them.",
            auto_repairable=repairable,
            details={
                "repair": "rebuild_trace_indexes" if repairable else None,
                "missing_indexes": missing_indexes,
                "orphan_indexes": orphan_indexes,
                "invalid_payloads": invalid_payloads,
            },
        )
    return _finding(
        context,
        "data_integrity.trace_indexes",
        "data-integrity",
        DiagnosticSeverity.INFO,
        "passed",
        "Trace index files are consistent.",
        f"checked={context.paths.traces_dir}",
        "No action needed.",
    )


def _json_files(context: DiagnosticContext) -> list[Path]:
    paths = [context.paths.config_file, context.paths.manifest_file]
    roots = [context.paths.traces_dir, context.paths.eval_dir]
    for root in roots:
        if root.exists():
            paths.extend(_bounded_files(root, ("*.json",)))
    eval_tasks = context.project_root / "tests" / "fixtures" / "evaluation"
    if eval_tasks.exists():
        paths.extend(_bounded_files(eval_tasks, ("*.json",)))
    return _dedupe_existing(paths)


def _jsonl_files(context: DiagnosticContext) -> list[Path]:
    memory_auto = context.project_root / ".singularity" / "memory" / "auto"
    paths = [
        memory_auto / "entries.jsonl",
        memory_auto / "candidates.jsonl",
    ]
    if context.paths.traces_dir.exists():
        paths.extend(_bounded_files(context.paths.traces_dir, ("*.jsonl",)))
    return _dedupe_existing(paths)


def _bounded_files(root: Path, patterns: tuple[str, ...], limit: int = 200) -> list[Path]:
    files: list[Path] = []
    for pattern in patterns:
        for path in root.rglob(pattern):
            if len(files) >= limit:
                return files
            if path.is_file() and path.stat().st_size <= 1_000_000:
                files.append(path)
    return files


def _dedupe_existing(paths: Iterable[Path]) -> list[Path]:
    seen: set[str] = set()
    result: list[Path] = []
    for path in paths:
        key = str(path)
        if key in seen or not path.exists() or not path.is_file():
            continue
        seen.add(key)
        result.append(path)
    return result


def _check_json_file(path: Path, issues: list[dict[str, str]]) -> None:
    try:
        text = path.read_text(encoding="utf-8")
        if not text.strip():
            issues.append({"path": str(path), "error": "empty JSON file"})
            return
        payload = json.loads(text)
        if not isinstance(payload, dict):
            issues.append({"path": str(path), "error": "expected JSON object"})
        if path.name == "singularity.json" and payload.get("schema_version") != CONFIG_SCHEMA_VERSION:
            issues.append({"path": str(path), "error": "unsupported config schema_version"})
        if path.name == "index.json" and payload.get("schema_version") not in {None, TASK_SET_SCHEMA_VERSION}:
            return
    except Exception as exc:
        issues.append({"path": str(path), "error": f"{type(exc).__name__}: {exc}"})


def _check_jsonl_file(path: Path, issues: list[dict[str, str]]) -> None:
    try:
        for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
            if not line.strip():
                continue
            payload = json.loads(line)
            if not isinstance(payload, dict):
                issues.append({"path": str(path), "error": f"line {line_number}: expected JSON object"})
    except Exception as exc:
        issues.append({"path": str(path), "error": f"{type(exc).__name__}: {exc}"})


def _jsonl_valid(path: Path) -> bool:
    try:
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                payload = json.loads(line)
                if not isinstance(payload, dict):
                    return False
    except Exception:
        return False
    return True


def _supports_python(spec: str) -> bool:
    spec = spec.strip()
    if spec.startswith(">="):
        version = tuple(int(part) for part in spec[2:].split(".")[:2])
        return sys.version_info[:2] >= version
    return True
