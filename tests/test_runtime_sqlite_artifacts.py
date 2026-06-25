from __future__ import annotations

import subprocess
from pathlib import Path, PurePosixPath

import pytest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
RUN_STATE_SQLITE_NAMES = {
    "context.sqlite3",
    "tool_protocol.sqlite3",
    "workspace_state.sqlite3",
    "index.sqlite",
}
RUN_STATE_DIR_PREFIXES = (
    ".singularity/",
    "outputs/",
    "test_run/",
    "work/traces/",
)
SQLITE_SIDECAR_SUFFIXES = (
    ".sqlite3-wal",
    ".sqlite3-shm",
    ".sqlite3-journal",
    ".sqlite-wal",
    ".sqlite-shm",
    ".sqlite-journal",
    ".db-wal",
    ".db-shm",
    ".db-journal",
)


def test_runtime_sqlite_state_is_not_tracked_by_git() -> None:
    if not (PROJECT_ROOT / ".git").exists():
        pytest.skip("Git metadata is required for tracked artifact regression checks.")

    tracked = _git(["ls-files", "-z"]).stdout.split("\0")
    offenders = sorted(
        path
        for path in tracked
        if path and (PROJECT_ROOT / path).exists() and _is_runtime_sqlite_state(path)
    )

    assert offenders == []


def test_runtime_sqlite_state_patterns_are_ignored() -> None:
    if not (PROJECT_ROOT / ".git").exists():
        pytest.skip("Git metadata is required for ignore regression checks.")

    examples = [
        "context.sqlite3",
        "tool_protocol.sqlite3",
        "test_run/context.sqlite3",
        "work/traces/runs/run_1/context.sqlite3",
        ".singularity/runs/run_1/tool_protocol.sqlite3",
        "context.sqlite3-wal",
    ]
    result = _git(["check-ignore", "--no-index", *examples])
    ignored = set(result.stdout.splitlines())

    assert set(examples).issubset(ignored)


def _is_runtime_sqlite_state(path: str) -> bool:
    normalized = path.replace("\\", "/")
    name = PurePosixPath(normalized).name
    if name in RUN_STATE_SQLITE_NAMES:
        return True
    if name.endswith(SQLITE_SIDECAR_SUFFIXES):
        return True
    if normalized.startswith(RUN_STATE_DIR_PREFIXES):
        return name.endswith((".sqlite", ".sqlite3", ".db"))
    return False


def _git(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=PROJECT_ROOT,
        text=True,
        capture_output=True,
        check=True,
    )
