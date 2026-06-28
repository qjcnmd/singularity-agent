from pathlib import Path
from subprocess import CompletedProcess, TimeoutExpired
from typing import Any

import singularity.workspace.git as workspace_git
from singularity.workspace.git import collect_git_state


def test_collect_git_state_uses_direct_git_without_command_executor(
    tmp_path: Path, monkeypatch
) -> None:
    calls: list[list[str]] = []

    def fake_run(command: list[str], **kwargs: Any) -> CompletedProcess[str]:
        calls.append(command)
        assert kwargs["cwd"] == tmp_path
        assert kwargs["env"]["GIT_TERMINAL_PROMPT"] == "0"
        assert kwargs["env"]["GIT_OPTIONAL_LOCKS"] == "0"
        if command == ["git", "rev-parse", "--is-inside-work-tree"]:
            return CompletedProcess(command, 0, "true\n", "")
        if command == ["git", "branch", "--show-current"]:
            return CompletedProcess(command, 0, "main\n", "")
        if command == ["git", "rev-parse", "HEAD"]:
            return CompletedProcess(command, 0, "abc123\n", "")
        if command == ["git", "status", "--porcelain=v1"]:
            return CompletedProcess(
                command,
                0,
                " M src/app.py\n"
                "?? notes.txt\n"
                "R  src/old.py -> src/new.py\n"
                "C  src/source.py -> src/copy.py\n",
                "",
            )
        raise AssertionError(f"unexpected git command: {command}")

    assert not hasattr(workspace_git, "CommandExecutor")
    monkeypatch.setattr(workspace_git.subprocess, "run", fake_run)
    state = collect_git_state(tmp_path)

    assert state.available is True
    assert state.branch == "main"
    assert state.head == "abc123"
    assert state.dirty_files == ["src/app.py", "notes.txt", "src/new.py", "src/copy.py"]
    assert state.staged_files == ["src/new.py", "src/copy.py"]
    assert state.untracked_files == ["notes.txt"]
    assert calls == [
        ["git", "rev-parse", "--is-inside-work-tree"],
        ["git", "branch", "--show-current"],
        ["git", "rev-parse", "HEAD"],
        ["git", "status", "--porcelain=v1"],
    ]


def test_collect_git_state_reports_unavailable_for_non_git_worktree(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_run(command: list[str], **_kwargs: Any) -> CompletedProcess[str]:
        assert command == ["git", "rev-parse", "--is-inside-work-tree"]
        return CompletedProcess(command, 0, "false\n", "")

    monkeypatch.setattr(workspace_git.subprocess, "run", fake_run)

    state = collect_git_state(tmp_path)

    assert state.available is False
    assert state.error == "not a git worktree"


def test_collect_git_state_reports_git_failure_without_raising(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_run(command: list[str], **_kwargs: Any) -> CompletedProcess[str]:
        return CompletedProcess(command, 128, "", "fatal: not a git repository\n")

    monkeypatch.setattr(workspace_git.subprocess, "run", fake_run)

    state = collect_git_state(tmp_path)

    assert state.available is False
    assert "fatal: not a git repository" in (state.error or "")


def test_collect_git_state_reports_timeout_without_raising(
    tmp_path: Path, monkeypatch
) -> None:
    def fake_run(command: list[str], **_kwargs: Any) -> CompletedProcess[str]:
        raise TimeoutExpired(command, timeout=5)

    monkeypatch.setattr(workspace_git.subprocess, "run", fake_run)

    state = collect_git_state(tmp_path)

    assert state.available is False
    assert "timed out" in (state.error or "")
