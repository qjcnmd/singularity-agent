#!/usr/bin/env python3
"""Shared helpers for local verification gate scripts."""

from __future__ import annotations

import json
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CommandSummary:
    name: str
    command: list[str]
    duration_seconds: float
    exit_code: int
    skipped: bool = False
    skipped_reason: str = ""

    @property
    def passed(self) -> bool:
        return self.skipped or self.exit_code == 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "command": self.command,
            "duration_seconds": self.duration_seconds,
            "exit_code": self.exit_code,
            "passed": self.passed,
            "skipped": self.skipped,
            "skipped_reason": self.skipped_reason,
        }


def run_command(name: str, command: list[str], *, cwd: Path) -> CommandSummary:
    started = time.perf_counter()
    completed = subprocess.run(command, cwd=cwd, check=False)
    return CommandSummary(
        name=name,
        command=command,
        duration_seconds=round(time.perf_counter() - started, 3),
        exit_code=completed.returncode,
    )


def skipped_command(name: str, reason: str) -> CommandSummary:
    return CommandSummary(
        name=name,
        command=[],
        duration_seconds=0.0,
        exit_code=0,
        skipped=True,
        skipped_reason=reason,
    )


def changed_python_scopes(changed_files: list[str]) -> list[str]:
    scopes: list[str] = []
    for raw_path in changed_files:
        path = Path(raw_path)
        if path.suffix != ".py":
            continue
        if not (path.exists() and (path.parts[:1] in {("src",), ("scripts",)} or path.parts[:2] == ("tests", "evaluation"))):
            continue
        if path.parts[:1] == ("src",):
            scopes.append(str(Path("src")))
        elif path.parts[:1] == ("scripts",):
            scopes.append(str(path))
    return sorted(dict.fromkeys(scopes))


def print_json_summary(summary: dict[str, Any]) -> None:
    print(json.dumps(summary, ensure_ascii=False, indent=2, sort_keys=True))


def repo_root_from_script(script_file: str) -> Path:
    return Path(script_file).resolve().parents[1]


def python() -> str:
    return sys.executable


def timing_summary(commands: list[CommandSummary], *, total_wall_time: float, extra: dict[str, Any] | None = None) -> dict[str, Any]:
    by_name = {command.name: command.duration_seconds for command in commands}
    summary: dict[str, Any] = {
        "total_wall_time_seconds": round(total_wall_time, 3),
        "ruff_time_seconds": by_name.get("ruff", 0.0),
        "mypy_time_seconds": by_name.get("mypy", 0.0),
        "compileall_time_seconds": by_name.get("compileall", by_name.get("compileall_changed_scope", 0.0)),
        "pytest_time_seconds": round(
            sum(command.duration_seconds for command in commands if "pytest" in command.name or command.name.endswith("_tests")),
            3,
        ),
        "runtime_docs_time_seconds": by_name.get("runtime_docs", 0.0),
        "capability_eval_time_seconds": by_name.get("public_representative_eval", 0.0),
        "provider_time_seconds": 0.0,
        "sandbox_time_seconds": 0.0,
        "verification_time_seconds": 0.0,
        "context_retrieval_compaction_time_seconds": 0.0,
        "selected_tests_count": 0,
        "skipped_tests_count": 0,
        "fallback_reason": "",
    }
    if extra:
        summary.update(extra)
    return summary


def capability_timing_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    provider = sandbox = verification = context = 0.0
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        capability = task.get("capability_summary") or {}
        if not isinstance(capability, dict):
            continue
        timing = task.get("timing") or capability.get("timing") or {}
        if isinstance(timing, dict):
            provider += float(timing.get("provider_time_seconds") or 0.0)
            sandbox += float(timing.get("sandbox_time_seconds") or 0.0)
            verification += float(timing.get("verification_time_seconds") or 0.0)
            context += float(timing.get("context_retrieval_compaction_time_seconds") or 0.0)
    return {
        "provider_time_seconds": round(provider, 3),
        "sandbox_time_seconds": round(sandbox, 3),
        "verification_time_seconds": round(verification, 3),
        "context_retrieval_compaction_time_seconds": round(context, 3),
    }


def capability_metrics_from_result(result_path: Path) -> dict[str, Any]:
    if not result_path.exists():
        return {}
    try:
        payload = json.loads(result_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    summary = payload.get("summary") or {}
    if not isinstance(summary, dict):
        summary = {}
    cost_sources: dict[str, int] = {}
    pricing_statuses: dict[str, int] = {}
    task_metrics_count = 0
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        metrics = task.get("evaluation_metrics") or {}
        if not isinstance(metrics, dict):
            continue
        task_metrics_count += 1
        cost = metrics.get("cost") or {}
        if not isinstance(cost, dict):
            continue
        source = str(cost.get("cost_source") or "unknown")
        pricing_status = str(cost.get("pricing_status") or "unknown")
        cost_sources[source] = cost_sources.get(source, 0) + 1
        pricing_statuses[pricing_status] = pricing_statuses.get(pricing_status, 0) + 1
    return {
        "resolved_count": int(summary.get("resolved_count") or 0),
        "resolved_rate": float(summary.get("resolved_rate") or 0.0),
        "total_cost_estimate": summary.get("total_cost_estimate"),
        "cost_per_resolved": summary.get("cost_per_resolved"),
        "average_tool_success_rate": summary.get("average_tool_success_rate"),
        "cost_sources": dict(sorted(cost_sources.items())),
        "pricing_statuses": dict(sorted(pricing_statuses.items())),
        "task_metrics_count": task_metrics_count,
    }
