from __future__ import annotations

import fnmatch
import re
from dataclasses import dataclass, field
from pathlib import Path

from singularity.memory.models import (
    Confidence,
    MemoryAuthorType,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
    Provenance,
    _now,
    digest_value,
)


@dataclass(frozen=True)
class PathScopedRule:
    id: str
    title: str
    body: str
    path: str
    patterns: list[str] = field(default_factory=list)
    created_at: str = field(default_factory=lambda: _now())

    @property
    def global_rule(self) -> bool:
        return not self.patterns

    def matches(self, paths: list[str]) -> bool:
        if self.global_rule:
            return True
        normalized_paths = [_normalize_path(path) for path in paths]
        if not normalized_paths:
            return False
        return any(
            _glob_match(path, pattern)
            for path in normalized_paths
            for pattern in self.patterns
        )

    def to_entry(self) -> MemoryEntry:
        return MemoryEntry(
            id=self.id,
            scope=MemoryScope.PROJECT,
            type=MemoryType.PROJECT_CONVENTION,
            source=MemorySource.HUMAN_FILE,
            title=self.title,
            body=self.body,
            confidence=Confidence.HIGH,
            provenance=Provenance(
                evidence=[
                    MemoryEvidenceRef(
                        source=MemorySource.HUMAN_FILE,
                        ref_id=self.path,
                        summary="path-scoped human rule",
                        path=self.path,
                        trust_level="trusted_operator",
                    )
                ]
            ),
            status=MemoryStatus.ACTIVE,
            author_type=MemoryAuthorType.HUMAN,
            paths=list(self.patterns),
            metadata={"memory_kind": "path_rule", "global_rule": self.global_rule},
        )

    def to_dict(self) -> dict[str, object]:
        return {
            "id": self.id,
            "title": self.title,
            "body": self.body,
            "path": self.path,
            "patterns": list(self.patterns),
            "global_rule": self.global_rule,
            "created_at": self.created_at,
        }


def load_rules(rules_dir: Path) -> list[PathScopedRule]:
    if not rules_dir.exists():
        return []
    rules: list[PathScopedRule] = []
    for path in sorted(rules_dir.glob("*.md")):
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        frontmatter, body = _split_frontmatter(text)
        patterns = [str(item) for item in _frontmatter_list(frontmatter, "paths")]
        cleaned_body = body.strip()
        if not cleaned_body:
            continue
        title = _title(cleaned_body, fallback=path.stem.replace("-", " ").title())
        rules.append(
            PathScopedRule(
                id=f"rule_{digest_value(str(path))[:12]}",
                title=title,
                body=cleaned_body,
                path=str(path),
                patterns=patterns,
            )
        )
    return rules


def _split_frontmatter(text: str) -> tuple[dict[str, object], str]:
    if not text.startswith("---"):
        return {}, text
    match = re.match(r"\A---\s*\n(?P<header>.*?)\n---\s*\n?(?P<body>.*)\Z", text, flags=re.DOTALL)
    if not match:
        return {}, text
    return _parse_frontmatter(match.group("header")), match.group("body")


def _parse_frontmatter(text: str) -> dict[str, object]:
    payload: dict[str, object] = {}
    current_key: str | None = None
    for raw_line in text.splitlines():
        line = raw_line.rstrip()
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith("  - ") and current_key:
            values = payload.setdefault(current_key, [])
            if isinstance(values, list):
                values.append(line[4:].strip().strip("'\""))
            continue
        if ":" in line:
            key, value = line.split(":", 1)
            current_key = key.strip()
            value = value.strip()
            if value.startswith("[") and value.endswith("]"):
                payload[current_key] = [
                    item.strip().strip("'\"")
                    for item in value[1:-1].split(",")
                    if item.strip()
                ]
            elif value:
                payload[current_key] = value.strip("'\"")
            else:
                payload[current_key] = []
    return payload


def _frontmatter_list(payload: dict[str, object], key: str) -> list[str]:
    value = payload.get(key)
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item) for item in value if str(item).strip()]
    return [str(value)] if str(value).strip() else []


def _title(text: str, *, fallback: str) -> str:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            return stripped.lstrip("#").strip() or fallback
        if stripped:
            return stripped[:80]
    return fallback


def _glob_match(path: str, pattern: str) -> bool:
    normalized_pattern = _normalize_path(pattern)
    return fnmatch.fnmatch(path, normalized_pattern) or fnmatch.fnmatch(path, normalized_pattern.rstrip("/") + "/**")


def _normalize_path(path: str) -> str:
    return path.replace("\\", "/").strip().lstrip("./")
