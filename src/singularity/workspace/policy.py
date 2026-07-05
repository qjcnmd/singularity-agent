from __future__ import annotations

from dataclasses import dataclass, field
from typing import ClassVar

from singularity.runtime.defaults import (
    WORKSPACE_MEDIUM_DIFF_LINE_THRESHOLD,
    WORKSPACE_REVIEW_DIFF_LINE_THRESHOLD,
)
from singularity.workspace_core import (
    BINARY,
    BUILD_SCRIPT,
    DEPENDENCY_LOCK,
    LARGE_ARTIFACT,
    PROJECT_CONFIG,
    SECRET,
    VCS_INTERNAL,
    FileClassifier,
    ResolvedWorkspacePath,
)

ALLOW = "allow"
DENY = "deny"
REQUIRE_REVIEW = "require_review"


@dataclass(frozen=True)
class PolicyDecision:
    decision: str
    file_class: str
    reasons: list[str] = field(default_factory=list)
    risk_tags: list[str] = field(default_factory=list)
    error_code: str | None = None


class WorkspacePolicy:
    denied_file_classes: ClassVar[set[str]] = {SECRET, VCS_INTERNAL, BINARY, LARGE_ARTIFACT}
    review_file_classes: ClassVar[set[str]] = {PROJECT_CONFIG, DEPENDENCY_LOCK, BUILD_SCRIPT}
    denied_dirs: ClassVar[set[str]] = {
        ".git",
        ".hg",
        ".singularity",
        ".svn",
        ".venv",
        "__pycache__",
        "node_modules",
        "venv",
    }

    def __init__(self, *, classifier: FileClassifier | None = None) -> None:
        self.classifier = classifier or FileClassifier()

    def check(
        self,
        *,
        operation_type: str,
        resolved: ResolvedWorkspacePath,
        size: int | None = None,
        is_binary: bool | None = None,
        added_lines: int = 0,
        removed_lines: int = 0,
        workspace_trust: str = "local",
        task_intent: str = "",
    ) -> PolicyDecision:
        file_class = self.classifier.classify(
            resolved,
            size=size,
            is_binary=is_binary,
        )
        risk_tags = self._risk_tags(
            operation_type=operation_type,
            file_class=file_class,
            added_lines=added_lines,
            removed_lines=removed_lines,
            workspace_trust=workspace_trust,
            task_intent=task_intent,
        )
        parts = {part.lower() for part in resolved.relative_path.parts}
        denied_parts = sorted(parts & self.denied_dirs)
        if denied_parts:
            return PolicyDecision(
                DENY,
                file_class,
                [f"Path is in denied workspace directory: {denied_parts[0]}"],
                risk_tags,
                "path_denied",
            )
        if file_class in self.denied_file_classes:
            return PolicyDecision(
                DENY,
                file_class,
                [f"File class is denied: {file_class}"],
                risk_tags,
                "file_class_denied",
            )
        if operation_type in {"DeleteFile", "MoveFile", "FormatFile"}:
            return PolicyDecision(
                REQUIRE_REVIEW,
                file_class,
                [f"{operation_type} requires review."],
                risk_tags,
                "review_required",
            )
        if file_class in self.review_file_classes:
            return PolicyDecision(
                REQUIRE_REVIEW,
                file_class,
                [f"File class requires review: {file_class}"],
                risk_tags,
                "review_required",
            )
        if added_lines + removed_lines > WORKSPACE_REVIEW_DIFF_LINE_THRESHOLD:
            return PolicyDecision(
                REQUIRE_REVIEW,
                file_class,
                ["Diff is large and requires review."],
                risk_tags,
                "review_required",
            )
        return PolicyDecision(ALLOW, file_class, ["Policy allowed mutation."], risk_tags)

    @staticmethod
    def _risk_tags(
        *,
        operation_type: str,
        file_class: str,
        added_lines: int,
        removed_lines: int,
        workspace_trust: str,
        task_intent: str,
    ) -> list[str]:
        tags = {"mutation", operation_type, file_class, f"trust:{workspace_trust}"}
        if added_lines or removed_lines:
            tags.add("diff")
        if added_lines + removed_lines > WORKSPACE_MEDIUM_DIFF_LINE_THRESHOLD:
            tags.add("large_diff")
        if task_intent:
            tags.add("task_intent")
        return sorted(tags)
