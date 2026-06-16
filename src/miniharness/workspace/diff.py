from __future__ import annotations

import difflib
import hashlib
from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class DiffHunk:
    header: str
    lines: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class FileDiff:
    path: str
    before_path: str | None
    after_path: str | None
    hunks: list[DiffHunk]
    added_lines: int
    removed_lines: int
    is_binary: bool = False
    is_rename: bool = False
    truncated: bool = False
    digest: str = ""
    artifact_path: str | None = None

    def summary(self) -> dict[str, object]:
        return {
            "path": self.path,
            "added_lines": self.added_lines,
            "removed_lines": self.removed_lines,
            "is_binary": self.is_binary,
            "is_rename": self.is_rename,
            "truncated": self.truncated,
            "diff_digest": self.digest,
            "artifact_path": self.artifact_path,
        }


class DiffEngine:
    def __init__(
        self,
        workspace_root: Path,
        *,
        context_lines: int = 3,
        max_inline_lines: int = 200,
    ) -> None:
        self.workspace_root = workspace_root
        self.context_lines = context_lines
        self.max_inline_lines = max_inline_lines

    def text_diff(
        self,
        *,
        path: str,
        before_text: str,
        after_text: str,
        before_path: str | None = None,
        after_path: str | None = None,
        is_rename: bool = False,
    ) -> FileDiff:
        diff_lines = list(
            difflib.unified_diff(
                before_text.splitlines(keepends=True),
                after_text.splitlines(keepends=True),
                fromfile=before_path or f"a/{path}",
                tofile=after_path or f"b/{path}",
                n=self.context_lines,
            )
        )
        added_lines = sum(
            1 for line in diff_lines if line.startswith("+") and not line.startswith("+++")
        )
        removed_lines = sum(
            1 for line in diff_lines if line.startswith("-") and not line.startswith("---")
        )
        full_diff = "".join(diff_lines)
        digest = hashlib.sha256(full_diff.encode("utf-8")).hexdigest()
        truncated = len(diff_lines) > self.max_inline_lines
        artifact_path = None
        inline_lines = diff_lines
        if truncated:
            artifact_path = self._write_artifact(digest, full_diff)
            inline_lines = diff_lines[: self.max_inline_lines]
        return FileDiff(
            path=path,
            before_path=before_path or path,
            after_path=after_path or path,
            hunks=self._parse_hunks(inline_lines),
            added_lines=added_lines,
            removed_lines=removed_lines,
            is_binary=False,
            is_rename=is_rename,
            truncated=truncated,
            digest=digest,
            artifact_path=artifact_path,
        )

    def binary_diff(
        self,
        *,
        path: str,
        before_path: str | None = None,
        after_path: str | None = None,
        is_rename: bool = False,
    ) -> FileDiff:
        payload = f"binary diff:{before_path}:{after_path}:{path}:{is_rename}"
        return FileDiff(
            path=path,
            before_path=before_path,
            after_path=after_path,
            hunks=[],
            added_lines=0,
            removed_lines=0,
            is_binary=True,
            is_rename=is_rename,
            digest=hashlib.sha256(payload.encode("utf-8")).hexdigest(),
        )

    def _write_artifact(self, digest: str, full_diff: str) -> str:
        artifact_dir = self.workspace_root / ".miniharness" / "artifacts" / "diffs"
        artifact_dir.mkdir(parents=True, exist_ok=True)
        artifact_path = artifact_dir / f"{digest}.diff"
        artifact_path.write_text(full_diff, encoding="utf-8")
        return artifact_path.relative_to(self.workspace_root).as_posix()

    @staticmethod
    def _parse_hunks(diff_lines: list[str]) -> list[DiffHunk]:
        hunks: list[DiffHunk] = []
        current: DiffHunk | None = None
        for line in diff_lines:
            stripped = line.rstrip("\n")
            if stripped.startswith("@@"):
                current = DiffHunk(header=stripped, lines=[])
                hunks.append(current)
                continue
            if current is not None:
                current.lines.append(stripped)
        return hunks
