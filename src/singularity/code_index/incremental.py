from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

from singularity.code_index.models import FreshnessStatus, IncrementalIndexResult


@dataclass(frozen=True)
class DirtySet:
    direct: list[str] = field(default_factory=list)
    reverse_dependencies: list[str] = field(default_factory=list)
    config: list[str] = field(default_factory=list)
    tests: list[str] = field(default_factory=list)
    docs: list[str] = field(default_factory=list)
    deleted: list[str] = field(default_factory=list)
    full_rebuild_required: bool = False
    reasons: dict[str, list[str]] = field(default_factory=dict)

    @property
    def rebuild_paths(self) -> list[str]:
        return sorted(set(self.direct + self.config + self.tests + self.docs))

    @property
    def stale_paths(self) -> list[str]:
        return sorted(set(self.reverse_dependencies) - set(self.rebuild_paths))


class IncrementalIndexer:
    def __init__(self, project_index: Any) -> None:
        self.component = project_index

    def update_after_changeset(
        self,
        *,
        changed_files: Iterable[str],
        deleted_files: Iterable[str] | None = None,
        reason: str = "changeset",
    ) -> IncrementalIndexResult:
        dirty = self.compute_dirty_set(changed_files=changed_files, deleted_files=deleted_files or [])
        for path in dirty.deleted:
            self.component.store.delete_by_path(path)
        if dirty.full_rebuild_required:
            return self.component.build_full_index(reason=reason)
        rebuilt = self.component._index_paths(dirty.rebuild_paths)
        if dirty.stale_paths:
            self.component.store.mark_stale(dirty.stale_paths, FreshnessStatus.STALE_DEPENDENCY)
        result = IncrementalIndexResult(
            changed_files=sorted(set(changed_files)),
            deleted_files=dirty.deleted,
            rebuilt_files=rebuilt,
            stale_files=dirty.stale_paths,
            dirty_reasons=dirty.reasons,
            full_rebuild_required=False,
            summary=self.component.store.load_summary().to_dict(),
            confidence=0.9,
            source="incremental_indexer",
        )
        self.component._emit_index_event("project_index.updated", result.to_dict())
        return result

    def compute_dirty_set(
        self,
        *,
        changed_files: Iterable[str],
        deleted_files: Iterable[str],
    ) -> DirtySet:
        changed = sorted({str(path).replace("\\", "/") for path in changed_files if str(path)})
        deleted = sorted({str(path).replace("\\", "/") for path in deleted_files if str(path)})
        reasons: dict[str, list[str]] = {path: ["direct_dirty"] for path in changed}
        for path in deleted:
            reasons.setdefault(path, []).append("deleted")
        config_dirty = [path for path in changed if Path(path).name in _CONFIG_NAMES]
        docs = [path for path in changed if Path(path).suffix.lower() in _DOC_SUFFIXES or "docs/" in path]
        tests = [path for path in changed if _is_test_path(path)]
        full = bool(config_dirty)
        reverse = []
        if changed:
            reverse = sorted({edge.importer_path for edge in self.component.store.query_reverse_dependencies(changed)})
            for path in reverse:
                reasons.setdefault(path, []).append("reverse_dependency_dirty")
        for path in config_dirty:
            reasons.setdefault(path, []).append("config_dirty")
        for path in tests:
            reasons.setdefault(path, []).append("test_mapping_dirty")
        for path in docs:
            reasons.setdefault(path, []).append("doc_dirty")
        return DirtySet(
            direct=changed,
            reverse_dependencies=reverse,
            config=config_dirty,
            tests=tests,
            docs=docs,
            deleted=deleted,
            full_rebuild_required=full,
            reasons=reasons,
        )


_CONFIG_NAMES = {
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "setup.cfg",
    "Cargo.toml",
    "tsconfig.json",
}
_DOC_SUFFIXES = {".md", ".mdx", ".rst", ".txt", ".adoc"}


def _is_test_path(path: str) -> bool:
    lowered = path.lower()
    return "/tests/" in f"/{lowered}" or Path(path).name.startswith("test_") or ".test." in lowered or ".spec." in lowered
