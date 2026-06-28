from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class GitState:
    available: bool
    branch: str | None = None
    head: str | None = None
    dirty_files: list[str] = field(default_factory=list)
    staged_files: list[str] = field(default_factory=list)
    untracked_files: list[str] = field(default_factory=list)
    error: str | None = None


def collect_git_state(workspace_root: Path) -> GitState:
    try:
        inside = _git(workspace_root, "rev-parse", "--is-inside-work-tree")
        if inside.strip().lower() != "true":
            return GitState(available=False, error="not a git worktree")
        branch = _git(workspace_root, "branch", "--show-current").strip() or None
        head = _git(workspace_root, "rev-parse", "HEAD").strip() or None
        status = _git(workspace_root, "status", "--porcelain=v1")
    except Exception as exc:
        return GitState(available=False, error=str(exc))

    dirty: list[str] = []
    staged: list[str] = []
    untracked: list[str] = []
    for line in status.splitlines():
        if not line:
            continue
        code = line[:2]
        path = _status_path(code, line[3:])
        dirty.append(path)
        if code == "??":
            untracked.append(path)
        elif code[0] != " ":
            staged.append(path)
    return GitState(
        available=True,
        branch=branch,
        head=head,
        dirty_files=dirty,
        staged_files=staged,
        untracked_files=untracked,
    )


def _git(workspace_root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=workspace_root,
        env={
            **os.environ,
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_OPTIONAL_LOCKS": "0",
        },
        text=True,
        capture_output=True,
        check=False,
        timeout=5,
    )
    if result.returncode != 0:
        message = (result.stderr or result.stdout or "git command failed").strip()
        raise RuntimeError(message or "git command failed")
    return result.stdout


def _status_path(code: str, raw_path: str) -> str:
    if "R" in code or "C" in code:
        _old, separator, new = raw_path.rpartition(" -> ")
        if separator:
            return new
    return raw_path
