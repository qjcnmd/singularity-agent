from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from singularity.command import CommandExecutor, CommandPurpose, CommandRequest


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
        path = line[3:]
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
    result = CommandExecutor(workspace_root).run(
        CommandRequest(
            argv=["git", *args],
            cwd=".",
            purpose=CommandPurpose.VCS_READ,
            timeout_seconds=5,
        )
    )
    if result.exit_code != 0:
        message = result.stderr_preview or result.stdout_preview or result.error_code
        raise RuntimeError(message or "git command failed")
    return result.stdout_preview
