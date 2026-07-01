#!/usr/bin/env python3
"""Fast local verification gate for routine Codex edits."""

from __future__ import annotations

import argparse
import json
import subprocess
import time
from pathlib import Path
from typing import Any

from verify_gate_common import (
    changed_python_scopes,
    print_json_summary,
    python,
    repo_root_from_script,
    run_command,
    skipped_command,
    timing_summary,
)


def _impact(args: argparse.Namespace, *, cwd: Path) -> tuple[dict[str, Any], float, int]:
    command = [python(), "scripts/test_impact.py", "--json"]
    if args.git or not args.files:
        command.append("--git")
        if args.base:
            command.extend(["--base", args.base])
    else:
        command.extend(args.files)
    started = time.perf_counter()
    completed = subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=False)
    duration = round(time.perf_counter() - started, 3)
    if completed.stdout.strip():
        try:
            return json.loads(completed.stdout), duration, completed.returncode
        except json.JSONDecodeError:
            pass
    return {
        "changed_files": args.files,
        "source": "test_impact_error",
        "warnings": [completed.stderr.strip() or completed.stdout.strip() or "test impact failed"],
        "recommended_tests": [],
        "recommended_commands": [],
        "confidence": "low",
        "fallback_gate": "stage",
        "skipped_reason": "test impact output was not valid JSON",
        "capability_gate": {"required": False, "areas": [], "files": [], "trigger": ""},
    }, duration, completed.returncode or 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the fast local verification gate.")
    parser.add_argument("files", nargs="*", help="Changed files. Defaults to git diff against HEAD.")
    parser.add_argument("--git", action="store_true", help="Use git changed files.")
    parser.add_argument("--base", default=None, help="Base ref for git diff.")
    args = parser.parse_args()

    cwd = repo_root_from_script(__file__)
    gate_started = time.perf_counter()
    impact, impact_duration, impact_exit = _impact(args, cwd=cwd)
    changed_files = [str(item) for item in impact.get("changed_files") or []]
    summaries = [
        run_command("ruff", [python(), "-m", "ruff", "check", "."], cwd=cwd),
        run_command("mypy", [python(), "-m", "mypy"], cwd=cwd),
    ]
    compile_scopes = changed_python_scopes(changed_files)
    if compile_scopes:
        summaries.append(run_command("compileall_changed_scope", [python(), "-m", "compileall", *compile_scopes], cwd=cwd))
    else:
        summaries.append(skipped_command("compileall_changed_scope", "no changed Python files under src/ or scripts/"))

    recommended_tests = [str(item) for item in impact.get("recommended_tests") or []]
    fallback_gate = impact.get("fallback_gate")
    skipped_reason = str(impact.get("skipped_reason") or "")
    if impact_exit != 0:
        fallback_gate = "stage"
        skipped_reason = skipped_reason or "test impact analysis failed"
    if fallback_gate:
        summaries.append(skipped_command("impacted_pytest", skipped_reason or "impact confidence requires stage fallback"))
    elif recommended_tests:
        summaries.append(
            run_command(
                "impacted_pytest",
                [
                    python(),
                    "-m",
                    "pytest",
                    *recommended_tests,
                    "-m",
                    "not provider_eval and not slow and not external",
                ],
                cwd=cwd,
            )
        )
    else:
        fallback_gate = "stage"
        skipped_reason = "No impacted pytest target could be selected"
        summaries.append(skipped_command("impacted_pytest", skipped_reason))

    commands = [item.to_dict() for item in summaries]
    passed = all(item.passed for item in summaries) and not fallback_gate and impact_exit == 0
    selected_tests_count = len(recommended_tests) if not fallback_gate else 0
    skipped_tests_count = 0 if selected_tests_count else 1
    fallback_reason = skipped_reason if fallback_gate else ""
    total_duration = round(time.perf_counter() - gate_started, 3)
    summary = {
        "gate": "fast",
        "passed": passed,
        "fallback_required": fallback_gate,
        "fallback_reason": fallback_reason,
        "stage_gate_recommended": bool(fallback_gate),
        "skipped_reason": skipped_reason,
        "selected_tests": recommended_tests if not fallback_gate else [],
        "selected_tests_count": selected_tests_count,
        "skipped_tests_count": skipped_tests_count,
        "impact": impact,
        "impact_duration_seconds": impact_duration,
        "commands": commands,
        "duration_seconds": total_duration,
        "timing": timing_summary(
            summaries,
            total_wall_time=total_duration,
            extra={
                "selected_tests_count": selected_tests_count,
                "skipped_tests_count": skipped_tests_count,
                "fallback_reason": fallback_reason,
            },
        ),
    }
    print_json_summary(summary)
    if any(not item.passed for item in summaries) or impact_exit != 0:
        return 1
    return 2 if fallback_gate else 0


if __name__ == "__main__":
    raise SystemExit(main())
