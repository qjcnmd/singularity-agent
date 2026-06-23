from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class GitStatus:
    available: bool
    workspace_root: str
    repository_root: str | None = None
    branch: str | None = None
    head: str | None = None
    is_dirty: bool = False
    entries: list[str] = field(default_factory=list)
    error: str | None = None

    def to_dict(self) -> dict[str, object]:
        return {
            "available": self.available,
            "workspace_root": self.workspace_root,
            "repository_root": self.repository_root,
            "branch": self.branch,
            "head": self.head,
            "is_dirty": self.is_dirty,
            "entries": self.entries,
            "error": self.error,
        }


@dataclass(frozen=True)
class GitDiffStat:
    available: bool
    staged: bool
    files: int = 0
    insertions: int = 0
    deletions: int = 0
    paths: list[str] = field(default_factory=list)
    raw: str = ""
    error: str | None = None

    def to_dict(self) -> dict[str, object]:
        return {
            "available": self.available,
            "staged": self.staged,
            "files": self.files,
            "insertions": self.insertions,
            "deletions": self.deletions,
            "paths": self.paths,
            "raw": self.raw,
            "error": self.error,
        }


@dataclass(frozen=True)
class GitCommitResult:
    ok: bool
    message: str
    files: list[str] = field(default_factory=list)
    commit: str | None = None
    exit_code: int = 0
    stdout: str = ""
    stderr: str = ""

    def to_dict(self) -> dict[str, object]:
        return {
            "ok": self.ok,
            "message": self.message,
            "files": self.files,
            "commit": self.commit,
            "exit_code": self.exit_code,
            "stdout": self.stdout,
            "stderr": self.stderr,
        }


class GitRuntime:
    """Small local Git control-plane wrapper.

    The runtime is intentionally local-only. It invokes the configured git
    executable directly, never pushes, and only stages paths explicitly scoped
    to the configured workspace root.
    """

    def __init__(
        self,
        workspace_root: Path | str,
        *,
        executable: str = "git",
        timeout_seconds: int = 30,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.executable = executable
        self.timeout_seconds = timeout_seconds

    def status(self) -> GitStatus:
        availability = self._run(["rev-parse", "--show-toplevel"])
        if availability.returncode != 0:
            return GitStatus(
                available=False,
                workspace_root=str(self.workspace_root),
                error=_combined_error(availability),
            )
        branch = self._run(["branch", "--show-current"])
        head = self._run(["rev-parse", "--short", "HEAD"])
        status = self._run(["status", "--short"])
        entries = [line for line in status.stdout.splitlines() if line.strip()]
        return GitStatus(
            available=True,
            workspace_root=str(self.workspace_root),
            repository_root=availability.stdout.strip() or None,
            branch=branch.stdout.strip() or None,
            head=head.stdout.strip() if head.returncode == 0 else None,
            is_dirty=bool(entries),
            entries=entries,
            error=_combined_error(status) if status.returncode != 0 else None,
        )

    def diff_stat(self, *, staged: bool = False) -> GitDiffStat:
        args = ["diff", "--numstat"]
        if staged:
            args.insert(1, "--cached")
        result = self._run(args)
        if result.returncode != 0:
            return GitDiffStat(
                available=False,
                staged=staged,
                raw=result.stdout,
                error=_combined_error(result),
            )
        insertions = 0
        deletions = 0
        paths: list[str] = []
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) < 3:
                continue
            insertions += _numstat_count(parts[0])
            deletions += _numstat_count(parts[1])
            paths.append(parts[2])
        return GitDiffStat(
            available=True,
            staged=staged,
            files=len(paths),
            insertions=insertions,
            deletions=deletions,
            paths=paths,
            raw=result.stdout,
        )

    def commit(
        self,
        message: str,
        *,
        paths: list[str] | None = None,
        allow_empty: bool = False,
    ) -> GitCommitResult:
        files = self._normalize_paths(paths)
        if files:
            added = self._run(["add", "--", *files])
            if added.returncode != 0:
                return GitCommitResult(
                    ok=False,
                    message=message,
                    files=files,
                    exit_code=added.returncode,
                    stdout=added.stdout,
                    stderr=added.stderr,
                )
        elif not allow_empty:
            return GitCommitResult(
                ok=False,
                message=message,
                files=[],
                exit_code=2,
                stderr="Explicit paths are required; refusing to stage the entire workspace.",
            )
        commit_args = ["commit", "-m", message]
        if allow_empty:
            commit_args.insert(1, "--allow-empty")
        committed = self._run(commit_args)
        commit_hash = None
        if committed.returncode == 0:
            head = self._run(["rev-parse", "--short", "HEAD"])
            commit_hash = head.stdout.strip() if head.returncode == 0 else None
        return GitCommitResult(
            ok=committed.returncode == 0,
            message=message,
            files=files,
            commit=commit_hash,
            exit_code=committed.returncode,
            stdout=committed.stdout,
            stderr=committed.stderr,
        )

    def _normalize_paths(self, paths: list[str] | None) -> list[str]:
        if not paths:
            return []
        normalized: list[str] = []
        root = self.workspace_root
        for item in paths:
            path = Path(item)
            candidate = path if path.is_absolute() else root / path
            resolved = candidate.resolve(strict=False)
            try:
                relative = resolved.relative_to(root)
            except ValueError as exc:
                raise ValueError(f"Git path is outside workspace: {item}") from exc
            normalized.append(relative.as_posix())
        return normalized

    def _run(self, args: list[str]) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.executable, *args],
            cwd=self.workspace_root,
            text=True,
            capture_output=True,
            timeout=self.timeout_seconds,
            check=False,
        )


def _numstat_count(value: str) -> int:
    return 0 if value == "-" else int(value)


def _combined_error(result: subprocess.CompletedProcess[str]) -> str:
    return (result.stderr or result.stdout or "").strip()
