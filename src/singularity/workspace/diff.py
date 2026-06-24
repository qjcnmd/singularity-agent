from __future__ import annotations

import difflib
import hashlib
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import TypedDict


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


@dataclass(frozen=True)
class UnifiedDiffFilePatch:
    path: str
    text: str
    is_new_file: bool = False
    is_delete: bool = False
    is_binary: bool = False
    is_rename: bool = False


class UnifiedDiffError(ValueError):
    def __init__(self, code: str, message: str, *, path: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.path = path


class _UnifiedHunk(TypedDict):
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    lines: list[str]


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
        artifact_dir = self.workspace_root / ".singularity" / "artifacts" / "diffs"
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


def parse_unified_diff(text: str) -> list[UnifiedDiffFilePatch]:
    lines = text.splitlines(keepends=True)
    if not lines:
        raise UnifiedDiffError("invalid_patch", "Patch is empty.")
    starts = [index for index, line in enumerate(lines) if line.startswith("diff --git ")]
    chunks: list[list[str]]
    if starts:
        chunks = [
            lines[start : starts[pos + 1] if pos + 1 < len(starts) else len(lines)]
            for pos, start in enumerate(starts)
        ]
    else:
        file_starts = [index for index, line in enumerate(lines) if line.startswith("--- ")]
        chunks = (
            [
                lines[start : file_starts[pos + 1] if pos + 1 < len(file_starts) else len(lines)]
                for pos, start in enumerate(file_starts)
            ]
            if file_starts
            else [lines]
        )
    patches = [_parse_file_patch(chunk) for chunk in chunks]
    if not patches:
        raise UnifiedDiffError("invalid_patch", "Patch contains no file hunks.")
    return patches


def apply_unified_diff_to_text(current: str, patch: str, *, path: str) -> str:
    file_patches = parse_unified_diff(patch)
    if len(file_patches) != 1:
        raise UnifiedDiffError("invalid_patch", "Expected one file patch.", path=path)
    file_patch = file_patches[0]
    if file_patch.is_binary or file_patch.is_rename or file_patch.is_delete:
        raise UnifiedDiffError("unsupported_operation", "Only text create/modify patches are supported.", path=path)
    if file_patch.is_new_file and current:
        raise UnifiedDiffError("file_changed", "Patch creates a file that already exists.", path=path)
    before_lines = current.splitlines(keepends=True)
    result: list[str] = []
    source_index = 0
    for hunk in _parse_unified_hunks(file_patch.text, path=path):
        start = max(hunk["old_start"] - 1, 0)
        if start < source_index:
            raise UnifiedDiffError("patch_context_not_found", "Unified diff hunks overlap.", path=path)
        result.extend(before_lines[source_index:start])
        source_index = start
        for line in hunk["lines"]:
            if line.startswith(" "):
                expected = line[1:]
                if source_index >= len(before_lines) or before_lines[source_index] != expected:
                    raise UnifiedDiffError("patch_context_not_found", "Unified diff context does not match.", path=path)
                result.append(before_lines[source_index])
                source_index += 1
            elif line.startswith("-"):
                expected = line[1:]
                if source_index >= len(before_lines) or before_lines[source_index] != expected:
                    raise UnifiedDiffError("patch_context_not_found", "Unified diff removal does not match.", path=path)
                source_index += 1
            elif line.startswith("+"):
                result.append(line[1:])
            elif line.startswith("\\"):
                continue
            else:
                raise UnifiedDiffError("invalid_patch", "Invalid unified diff line.", path=path)
    result.extend(before_lines[source_index:])
    return "".join(result)


def _parse_file_patch(lines: list[str]) -> UnifiedDiffFilePatch:
    text = "".join(lines)
    if any(line.startswith(("Binary files ", "GIT binary patch")) for line in lines):
        raise UnifiedDiffError("unsupported_operation", "Binary patches are not supported.")
    if any(line.startswith(("rename from ", "rename to ")) for line in lines):
        raise UnifiedDiffError("unsupported_operation", "Rename patches are not supported.")
    if not any(line.startswith("@@") for line in lines):
        raise UnifiedDiffError("invalid_patch", "Patch contains no file hunks.")
    old_header = next((line for line in lines if line.startswith("--- ")), "")
    new_header = next((line for line in lines if line.startswith("+++ ")), "")
    if not old_header or not new_header:
        raise UnifiedDiffError("invalid_patch", "Patch is missing file headers.")
    old_path = _header_path(old_header[4:].strip())
    new_path = _header_path(new_header[4:].strip())
    if old_path == "/dev/null" and new_path == "/dev/null":
        raise UnifiedDiffError("invalid_patch", "Patch has no target path.")
    target = new_path if new_path != "/dev/null" else old_path
    if target.startswith("/") or target.startswith("../") or "/../" in target:
        raise UnifiedDiffError("path_outside_workspace", "Patch target is outside workspace.", path=target)
    return UnifiedDiffFilePatch(
        path=target,
        text=text,
        is_new_file=old_path == "/dev/null",
        is_delete=new_path == "/dev/null",
    )


def _parse_unified_hunks(diff: str, *, path: str) -> list[_UnifiedHunk]:
    hunks: list[_UnifiedHunk] = []
    current: _UnifiedHunk | None = None
    header = re.compile(
        r"^@@ -(?P<old_start>\d+)(?:,(?P<old_count>\d+))? "
        r"\+(?P<new_start>\d+)(?:,(?P<new_count>\d+))? @@"
    )
    for line in diff.splitlines(keepends=True):
        if line.startswith(("--- ", "+++ ", "diff ", "index ", "new file mode ")):
            continue
        match = header.match(line)
        if match:
            current = {
                "old_start": int(match.group("old_start")),
                "old_count": int(match.group("old_count") or 1),
                "new_start": int(match.group("new_start")),
                "new_count": int(match.group("new_count") or 1),
                "lines": [],
            }
            hunks.append(current)
            continue
        if current is None:
            continue
        if line.startswith((" ", "+", "-", "\\")):
            current["lines"].append(line)
    if not hunks:
        raise UnifiedDiffError("invalid_patch", "Unified diff contains no hunks.", path=path)
    return hunks


def _header_path(value: str) -> str:
    path = value.split("\t", 1)[0].split(" ", 1)[0]
    if path in {"/dev/null", "dev/null"}:
        return "/dev/null"
    if path.startswith(("a/", "b/")):
        return path[2:]
    return path
