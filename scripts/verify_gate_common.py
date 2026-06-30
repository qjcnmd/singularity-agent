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
