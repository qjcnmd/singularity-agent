from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from miniharness.workspace.pathing import ResolvedWorkspacePath


PUBLIC_SOURCE = "PUBLIC_SOURCE"
PROJECT_CONFIG = "PROJECT_CONFIG"
TEST = "TEST"
DOCUMENTATION = "DOCUMENTATION"
BUILD_SCRIPT = "BUILD_SCRIPT"
DEPENDENCY_LOCK = "DEPENDENCY_LOCK"
SECRET = "SECRET"
VCS_INTERNAL = "VCS_INTERNAL"
GENERATED = "GENERATED"
BINARY = "BINARY"
LARGE_ARTIFACT = "LARGE_ARTIFACT"
UNKNOWN = "UNKNOWN"

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


class FileClassifier:
    binary_extensions = {
        ".7z",
        ".avif",
        ".bin",
        ".bmp",
        ".class",
        ".dll",
        ".exe",
        ".gif",
        ".ico",
        ".jar",
        ".jpeg",
        ".jpg",
        ".pdf",
        ".png",
        ".pyc",
        ".so",
        ".webp",
        ".zip",
    }
    source_extensions = {
        ".c",
        ".cpp",
        ".cs",
        ".css",
        ".go",
        ".html",
        ".java",
        ".js",
        ".jsx",
        ".json",
        ".mdx",
        ".py",
        ".rs",
        ".sh",
        ".sql",
        ".ts",
        ".tsx",
    }
    docs_extensions = {".adoc", ".md", ".rst", ".txt"}
    config_names = {
        ".editorconfig",
        ".flake8",
        ".gitignore",
        ".pre-commit-config.yaml",
        "hatch.toml",
        "mypy.ini",
        "pyproject.toml",
        "pytest.ini",
        "ruff.toml",
        "setup.cfg",
        "setup.py",
        "tox.ini",
        "tsconfig.json",
    }
    lock_names = {
        "cargo.lock",
        "go.sum",
        "package-lock.json",
        "pnpm-lock.yaml",
        "poetry.lock",
        "requirements.txt",
        "uv.lock",
        "yarn.lock",
    }
    build_names = {"dockerfile", "makefile", "justfile"}
    generated_dirs = {
        ".coverage",
        ".deepeval",
        ".miniharness",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        "__pycache__",
        "build",
        "coverage",
        "dist",
        "outputs",
    }

    def __init__(self, *, large_file_bytes: int = 1_000_000) -> None:
        self.large_file_bytes = large_file_bytes

    def classify(
        self,
        resolved: ResolvedWorkspacePath,
        *,
        size: int | None = None,
        is_binary: bool | None = None,
    ) -> str:
        path = resolved.relative_path
        parts = tuple(part.lower() for part in path.parts)
        name = path.name
        lower_name = name.lower()
        suffix = path.suffix.lower()

        if ".git" in parts:
            return VCS_INTERNAL
        if self._is_secret_name(name):
            return SECRET
        if is_binary or suffix in self.binary_extensions:
            return BINARY
        if size is not None and size > self.large_file_bytes:
            return LARGE_ARTIFACT
        if any(part in self.generated_dirs for part in parts):
            return GENERATED
        if lower_name in self.lock_names:
            return DEPENDENCY_LOCK
        if lower_name in self.build_names or suffix in {".ps1", ".bat", ".cmd"}:
            return BUILD_SCRIPT
        if lower_name in self.config_names or suffix in {".ini", ".cfg", ".toml", ".yaml", ".yml"}:
            return PROJECT_CONFIG
        if "tests" in parts or "test" in parts or lower_name.startswith("test_"):
            return TEST
        if "docs" in parts or suffix in self.docs_extensions:
            return DOCUMENTATION
        if suffix in self.source_extensions:
            return PUBLIC_SOURCE
        return UNKNOWN

    @staticmethod
    def _is_secret_name(name: str) -> bool:
        lower = name.lower()
        if lower == ".env":
            return True
        if lower.startswith(".env.") and lower != ".env.example":
            return True
        return lower.endswith((".pem", ".key", ".p12", ".pfx"))


class WorkspacePolicy:
    denied_file_classes = {SECRET, VCS_INTERNAL, BINARY, LARGE_ARTIFACT}
    review_file_classes = {PROJECT_CONFIG, DEPENDENCY_LOCK, BUILD_SCRIPT}
    denied_dirs = {
        ".git",
        ".hg",
        ".miniharness",
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
        if added_lines + removed_lines > 500:
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
        if added_lines + removed_lines > 100:
            tags.add("large_diff")
        if task_intent:
            tags.add("task_intent")
        return sorted(tags)
