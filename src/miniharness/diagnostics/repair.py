from __future__ import annotations

import json
import os
from dataclasses import replace
from pathlib import Path
from typing import Any
from uuid import uuid4

from miniharness.code_index import ProjectIndexRuntime
from miniharness.memory.store import MemoryStore
from miniharness.release.init import default_config, initialize_runtime
from miniharness.release.migrations import apply_migrations
from miniharness.release.models import atomic_write_json, read_json
from miniharness.release.paths import RuntimePaths

from miniharness.diagnostics.models import (
    DiagnosticFinding,
    DiagnosticResult,
    DiagnosticSeverity,
    RepairAction,
    RepairPlan,
    now_iso,
)


class RepairEngine:
    def run(
        self,
        result: DiagnosticResult,
        *,
        paths: RuntimePaths,
        project_root: Path,
        apply: bool = False,
    ) -> RepairPlan:
        actions, blocked = self._actions_for(result)
        if not apply or not actions:
            return RepairPlan(actions=actions, blocked_actions=blocked, applied=False)
        audit_log = self._write_audit(paths, actions)
        applied_actions: list[RepairAction] = []
        for action in actions:
            try:
                self._apply_action(action, paths=paths, project_root=project_root)
                applied_actions.append(replace(action, status="applied", message="applied"))
            except Exception as exc:
                applied_actions.append(
                    replace(
                        action,
                        status="failed",
                        message=f"{type(exc).__name__}: {exc}",
                    )
                )
        return RepairPlan(
            actions=applied_actions,
            blocked_actions=blocked,
            applied=True,
            audit_log_path=str(audit_log),
        )

    def _actions_for(self, result: DiagnosticResult) -> tuple[list[RepairAction], list[dict[str, Any]]]:
        actions: list[RepairAction] = []
        blocked: list[dict[str, Any]] = []
        seen: set[tuple[str, str]] = set()
        allow_suggestions = bool(result.filters.get("check_id"))
        for finding in result.findings:
            if finding.status != "failed":
                continue
            repair_kind = finding.details.get("repair")
            if not finding.auto_repairable or not repair_kind:
                blocked.append(
                    {
                        "check_id": finding.check_id,
                        "reason": "not_auto_repairable",
                        "suggested_fix": finding.suggested_fix,
                    }
                )
                continue
            if finding.severity == DiagnosticSeverity.SUGGESTION and not allow_suggestions:
                blocked.append(
                    {
                        "check_id": finding.check_id,
                        "reason": "suggestion_requires_explicit_check",
                        "suggested_fix": f"Run miniharness repair --apply --check {finding.check_id}.",
                    }
                )
                continue
            target = str(finding.details.get("path") or finding.details.get("missing") or finding.check_id)
            key = (str(repair_kind), target)
            if key in seen:
                continue
            seen.add(key)
            actions.append(
                RepairAction(
                    action_id=f"repair_{uuid4().hex[:12]}",
                    check_id=finding.check_id,
                    description=self._description(str(repair_kind), finding),
                    risk="low",
                    kind=str(repair_kind),
                    target=target,
                    params=dict(finding.details),
                )
            )
        return actions, blocked

    @staticmethod
    def _description(kind: str, finding: DiagnosticFinding) -> str:
        return {
            "create_dirs": "Create missing MiniHarness runtime/workspace directories.",
            "write_default_config": "Write default MiniHarness config file.",
            "merge_default_config": "Merge missing default config fields without overwriting custom values.",
            "write_manifest": "Create missing runtime manifest and defaults.",
            "apply_migrations": "Apply pending runtime migrations with existing backup flow.",
            "rebuild_memory_index": "Rebuild derived memory index without deleting memory entries.",
            "rebuild_project_index": "Rebuild derived project index cache.",
            "rebuild_trace_indexes": "Rebuild missing trace index files from trace metadata.",
        }.get(kind, finding.suggested_fix)

    def _apply_action(self, action: RepairAction, *, paths: RuntimePaths, project_root: Path) -> None:
        if action.kind == "create_dirs":
            for raw_path in action.params.get("missing") or action.params.get("paths") or []:
                Path(raw_path).mkdir(parents=True, exist_ok=True)
            return
        if action.kind == "write_default_config":
            for directory in paths.directories():
                directory.mkdir(parents=True, exist_ok=True)
            if not paths.config_file.exists():
                atomic_write_json(paths.config_file, default_config(paths))
            return
        if action.kind == "merge_default_config":
            self._merge_default_config(paths)
            return
        if action.kind == "write_manifest":
            initialize_runtime(paths, force=False)
            return
        if action.kind == "apply_migrations":
            apply_migrations(paths)
            return
        if action.kind == "rebuild_memory_index":
            store = MemoryStore(project_root)
            store.initialize(rebuild_index=True)
            store.rebuild_index()
            return
        if action.kind == "rebuild_project_index":
            ProjectIndexRuntime(project_root).build_full_index(reason="diagnostic_repair")
            return
        if action.kind == "rebuild_trace_indexes":
            self._rebuild_trace_indexes(paths)
            return
        raise ValueError(f"Unsupported repair action: {action.kind}")

    @staticmethod
    def _merge_default_config(paths: RuntimePaths) -> None:
        paths.config_dir.mkdir(parents=True, exist_ok=True)
        current = read_json(paths.config_file) if paths.config_file.exists() else {}
        merged = _deep_merge_missing(current, default_config(paths))
        atomic_write_json(paths.config_file, merged)

    @staticmethod
    def _rebuild_trace_indexes(paths: RuntimePaths) -> None:
        if not paths.traces_dir.exists():
            return
        for run_dir in paths.traces_dir.iterdir():
            if not run_dir.is_dir():
                continue
            index = run_dir / "index.json"
            if index.exists():
                continue
            payload = {
                "run_id": run_dir.name,
                "events": "events.jsonl",
                "spans": "spans.jsonl",
                "artifacts": "artifacts.jsonl",
                "created_at": now_iso(),
            }
            atomic_write_json(index, payload)

    @staticmethod
    def _write_audit(paths: RuntimePaths, actions: list[RepairAction]) -> Path:
        paths.logs_dir.mkdir(parents=True, exist_ok=True)
        audit_log = paths.logs_dir / "repair-audit.jsonl"
        payload = {
            "schema_version": "repair-audit/v1",
            "created_at": now_iso(),
            "actions": [action.to_dict() for action in actions],
        }
        with audit_log.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        return audit_log


def _deep_merge_missing(current: dict[str, Any], defaults: dict[str, Any]) -> dict[str, Any]:
    merged = dict(current)
    for key, value in defaults.items():
        if key not in merged:
            merged[key] = value
        elif isinstance(merged[key], dict) and isinstance(value, dict):
            merged[key] = _deep_merge_missing(merged[key], value)
    return merged
