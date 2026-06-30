#!/usr/bin/env python3
"""Capability gate for the single public representative real task."""

from __future__ import annotations

import argparse
import json
import subprocess
import time
from pathlib import Path
from typing import Any

from verify_gate_common import (
    print_json_summary,
    python,
    repo_root_from_script,
    run_command,
)

DEFAULT_MANIFEST = "docs/evaluation/public-representative-task.json"
DEFAULT_RUN_ID = "public-long-task-gate"


def _impact(files: list[str], *, cwd: Path) -> dict[str, Any]:
    command = [python(), "scripts/test_impact.py", "--json"]
    if files:
        command.extend(files)
    else:
        command.append("--git")
    completed = subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=False)
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return {
            "changed_files": files,
            "warnings": [completed.stderr.strip() or completed.stdout.strip() or "test impact failed"],
            "capability_gate": {"required": False, "areas": [], "files": [], "trigger": ""},
        }


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the public representative capability gate.")
    parser.add_argument("files", nargs="*", help="Changed files for trigger detection.")
    parser.add_argument("--force", action="store_true", help="Run even when no core-chain trigger is detected.")
    parser.add_argument("--manifest", default=DEFAULT_MANIFEST, help="Evaluation manifest path.")
    parser.add_argument("--run-id", default=DEFAULT_RUN_ID, help="Evaluation run id.")
    args = parser.parse_args()

    cwd = repo_root_from_script(__file__)
    started = time.perf_counter()
    impact = _impact(args.files, cwd=cwd)
    capability_gate = impact.get("capability_gate") or {}
    if not args.force and not capability_gate.get("required"):
        print_json_summary(
            {
                "gate": "capability",
                "passed": True,
                "ran": False,
                "skipped_reason": "no AgentLoop/ToolProtocol/sandbox/context/compaction/verification/CompletionGate/FinalReport/evaluation runner changes detected",
                "impact": impact,
                "commands": [],
                "duration_seconds": round(time.perf_counter() - started, 3),
            }
        )
        return 0

    command = [
        python(),
        "-m",
        "singularity.cli",
        "eval",
        "run",
        args.manifest,
        "--run-id",
        args.run_id,
        "--json",
    ]
    result = run_command("public_representative_eval", command, cwd=cwd)
    print_json_summary(
        {
            "gate": "capability",
            "passed": result.passed,
            "ran": True,
            "manifest": args.manifest,
            "run_id": args.run_id,
            "impact": impact,
            "commands": [result.to_dict()],
            "duration_seconds": round(time.perf_counter() - started, 3),
        }
    )
    return 0 if result.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
