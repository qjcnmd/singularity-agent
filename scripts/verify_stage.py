#!/usr/bin/env python3
"""Deterministic stage-close verification gate."""

from __future__ import annotations

import time

from verify_gate_common import print_json_summary, python, repo_root_from_script, run_command, timing_summary


def main() -> int:
    cwd = repo_root_from_script(__file__)
    started = time.perf_counter()
    commands = [
        run_command("mypy", [python(), "-m", "mypy"], cwd=cwd),
        run_command("ruff", [python(), "-m", "ruff", "check", "."], cwd=cwd),
        run_command("compileall", [python(), "-m", "compileall", "src", "scripts"], cwd=cwd),
        run_command("runtime_docs", [python(), "scripts/verify_runtime_docs.py"], cwd=cwd),
        run_command(
            "deterministic_pytest",
            [
                python(),
                "-m",
                "pytest",
                "tests/",
                "-x",
                "-m",
                "not evaluation and not provider_eval and not slow and not external",
            ],
            cwd=cwd,
        ),
        run_command(
            "evaluation_runner_tests",
            [
                python(),
                "-m",
                "pytest",
                "tests/evaluation/test_evaluation_runner.py",
                "-m",
                "evaluation and not provider_eval and not slow and not external",
            ],
            cwd=cwd,
        ),
        run_command("test_impact_tests", [python(), "-m", "pytest", "tests/test_test_impact.py"], cwd=cwd),
        run_command("quality_gate_tests", [python(), "-m", "pytest", "tests/test_quality_gates.py"], cwd=cwd),
    ]
    passed = all(command.passed for command in commands)
    duration = round(time.perf_counter() - started, 3)
    print_json_summary(
        {
            "gate": "stage",
            "passed": passed,
            "commands": [command.to_dict() for command in commands],
            "duration_seconds": duration,
            "timing": timing_summary(
                commands,
                total_wall_time=duration,
                extra={
                    "selected_tests_count": 4,
                    "skipped_tests_count": 0,
                    "fallback_reason": "",
                },
            ),
            "real_provider_eval": {
                "run": False,
                "skipped_reason": "stage gate is deterministic and does not run real provider evaluation by default",
            },
        }
    )
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
