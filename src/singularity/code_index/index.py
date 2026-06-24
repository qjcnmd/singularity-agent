from __future__ import annotations

import os
import time
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable
from uuid import uuid4

from singularity.code_index.context import build_project_index_observation
from singularity.code_index.impact import ProjectImpactAnalyzer
from singularity.code_index.incremental import IncrementalIndexer
from singularity.code_index.language import LanguagePlugin
from singularity.code_index.models import (
    SCHEMA_VERSION,
    BackendInfo,
    CallEdgeRecord,
    CodeImpactAnalysis,
    ConfigFactRecord,
    DependencyEdgeRecord,
    DocSectionRecord,
    EntryPointRecord,
    Evidence,
    FileRecord,
    IndexSummary,
    IncrementalIndexResult,
    ProjectRootRecord,
    ReferenceRecord,
    SymbolRecord,
    TestImpactAnalysis,
    TestMappingRecord,
    TrustLevel,
)
from singularity.code_index.plugins import (
    JavaScriptPlugin,
    PythonPlugin,
    RustPlugin,
    TypeScriptPlugin,
)
from singularity.code_index.query import ProjectIndexQueryService
from singularity.code_index.scanner import ScannerBudget, WorkspaceScanner
from singularity.code_index.store import ProjectIndexStore
from singularity.observability.models import TraceEventType, TraceSeverity


@dataclass(frozen=True)
class ProjectIndexConfig:
    enabled: bool = True
    db_path: Path | None = None
    build_on_boot: bool = True
    max_files: int = 20_000
    max_file_size: int = 1_000_000
    max_total_bytes: int = 50_000_000


class ProjectIndex:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        db_path: Path | str | None = None,
        trace: Any | None = None,
        config: ProjectIndexConfig | None = None,
        plugins: list[LanguagePlugin] | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.config = config or ProjectIndexConfig(db_path=Path(db_path) if db_path else None)
        self.db_path = Path(db_path) if db_path else self.config.db_path or self.workspace_root / ".singularity" / "index.sqlite"
        self.trace = trace
        self._store: ProjectIndexStore | None = None
        self.plugins = plugins or [PythonPlugin(), JavaScriptPlugin(), TypeScriptPlugin(), RustPlugin()]
        self.scanner = WorkspaceScanner(
            self.workspace_root,
            budget=ScannerBudget(
                max_files=self.config.max_files,
                max_file_size=self.config.max_file_size,
                max_total_bytes=self.config.max_total_bytes,
            ),
        )
        self._query_service: ProjectIndexQueryService | None = None
        self._impact_analyzer: ProjectImpactAnalyzer | None = None
        self._incremental: IncrementalIndexer | None = None
        self.index_id = f"index_{uuid4().hex[:12]}"
        self._bootstrapped = False

    @property
    def store(self) -> ProjectIndexStore:
        if self._store is None:
            self._store = ProjectIndexStore(self.db_path)
        return self._store

    @store.setter
    def store(self, value: ProjectIndexStore) -> None:
        self._store = value
        self._query_service = None
        self._impact_analyzer = None

    @property
    def query_service(self) -> ProjectIndexQueryService:
        if self._query_service is None:
            self._query_service = ProjectIndexQueryService(self.store)
        return self._query_service

    @property
    def impact_analyzer(self) -> ProjectImpactAnalyzer:
        if self._impact_analyzer is None:
            self._impact_analyzer = ProjectImpactAnalyzer(self.store)
        return self._impact_analyzer

    @property
    def incremental(self) -> IncrementalIndexer:
        if self._incremental is None:
            self._incremental = IncrementalIndexer(self)
        return self._incremental

    def bootstrap(self, *, reason: str = "kernel_boot") -> IndexSummary:
        if not self.config.enabled:
            self._bootstrapped = True
            return self._disabled_summary()
        if self.config.build_on_boot or self.store.load_summary().file_count == 0:
            return self.build_full_index(reason=reason)
        summary = self.refresh(reason=reason)
        self._bootstrapped = True
        return summary

    def build_full_index(self, *, reason: str = "manual") -> IndexSummary:
        started = time.perf_counter()
        self._emit(
            TraceEventType.PROJECT_INDEX_BUILD_STARTED,
            "Project index build started.",
            {"reason": reason, "schema_version": SCHEMA_VERSION},
        )
        try:
            files = self.scanner.scan()
            project_roots, entrypoints, config_facts, symbols, dependencies, references, call_edges, tests, docs = self._extract_facts(files)
            dependencies = self._resolve_dependency_paths(dependencies, files)
            build_store = self._temporary_build_store()
            build_store.reset()
            build_store.upsert_files(files)
            build_store.upsert_project_roots(project_roots)
            build_store.upsert_entrypoints(entrypoints)
            build_store.upsert_config_facts(config_facts)
            build_store.upsert_symbols(symbols)
            build_store.upsert_dependencies(dependencies)
            build_store.upsert_references(references)
            build_store.upsert_call_edges(call_edges)
            build_store.upsert_test_mappings(tests)
            build_store.upsert_doc_sections(docs)
            build_store.set_metadata("schema_version", SCHEMA_VERSION)
            build_store.set_metadata("plugin_versions", {plugin.name: plugin.version for plugin in self.plugins})
            self._promote_build_store(build_store)
            summary = self.store.load_summary()
            self._bootstrapped = True
            self._emit(
                TraceEventType.PROJECT_INDEX_BUILD_COMPLETED,
                "Project index build completed.",
                {
                    "reason": reason,
                    "duration_ms": int((time.perf_counter() - started) * 1000),
                    "summary": _bounded_summary(summary),
                },
            )
            return summary
        except Exception as exc:
            self._emit(
                TraceEventType.PROJECT_INDEX_BUILD_FAILED,
                "Project index build failed.",
                {
                    "reason": reason,
                    "type": type(exc).__name__,
                    "message": str(exc),
                },
                severity=TraceSeverity.WARNING,
            )
            raise

    def _temporary_build_store(self) -> ProjectIndexStore:
        tmp_path = self.db_path.with_name(f".{self.db_path.name}.{uuid4().hex}.tmp")
        if tmp_path.exists():
            tmp_path.unlink()
        return ProjectIndexStore(tmp_path)

    def _promote_build_store(self, build_store: ProjectIndexStore) -> None:
        os.replace(build_store.path, self.db_path)
        self.store = ProjectIndexStore(self.db_path)

    def refresh(self, *, reason: str = "manual") -> IndexSummary:
        if not self.config.enabled:
            return self._disabled_summary()
        current = {file.path: file for file in self.scanner.scan()}
        indexed = {file.path: file for file in self.store.all_files()}
        changed = [
            path
            for path, file in current.items()
            if path not in indexed
            or indexed[path].sha256 != file.sha256
            or indexed[path].mtime_ns != file.mtime_ns
        ]
        deleted = sorted(set(indexed) - set(current))
        if changed or deleted:
            self.update_after_changeset({"changed_files": changed, "deleted_files": deleted}, reason=reason)
        summary = self.store.load_summary()
        self._emit_index_event("project_index.refreshed", {"reason": reason, "summary": _bounded_summary(summary)})
        return summary

    def update_after_changeset(self, changeset: Any, *, reason: str = "changeset") -> Any:
        changed, deleted = self._changed_and_deleted_from(changeset)
        if not self.config.enabled:
            return IncrementalIndexResult(
                changed_files=changed,
                deleted_files=deleted,
                summary=_bounded_summary(self._disabled_summary()),
            )
        return self.incremental.update_after_changeset(
            changed_files=changed,
            deleted_files=deleted,
            reason=reason,
        )

    def find_relevant_files(self, goal: str, hints: Iterable[str] | None = None):
        if not self.config.enabled:
            return []
        return self.query_service.find_relevant_files(goal, hints)

    def find_symbols(self, query: str):
        if not self.config.enabled:
            return []
        return self.query_service.find_symbols(query)

    def get_context_candidates(self, goal: str, budget_tokens: int = 4000, hints: Iterable[str] | None = None):
        if not self.config.enabled:
            return []
        return self.query_service.get_context_candidates(goal, budget_tokens=budget_tokens, hints=hints)

    def analyze_impact(self, paths: Iterable[str]):
        paths = list(paths)
        if not self.config.enabled:
            return CodeImpactAnalysis(
                requested_paths=paths,
                risk_level="unknown",
                risk_reasons=["project_index_disabled"],
                recommended_validation=["Run relevant tests manually because ProjectIndex is disabled."],
            )
        return self.impact_analyzer.analyze_paths(paths)

    def get_test_impact(self, changed_files: Iterable[str]):
        changed_files = list(changed_files)
        if not self.config.enabled:
            return TestImpactAnalysis(
                changed_files=changed_files,
                require_full_test=True,
                confidence_note="ProjectIndex is disabled.",
            )
        return self.impact_analyzer.get_test_impact(changed_files)

    def explain(self) -> dict[str, object]:
        if not self.config.enabled:
            summary = self._disabled_summary()
            return {
                "summary": summary.to_dict(),
                "project_roots": [],
                "entrypoints": [],
                "limitations": summary.limitations,
            }
        return self.query_service.explain_project_structure()

    def observation_for_goal(
        self,
        goal: str,
        *,
        hints: Iterable[str] | None = None,
        budget_tokens: int = 3000,
    ) -> dict[str, Any]:
        if not self.config.enabled:
            return build_project_index_observation(
                index_id=self.index_id,
                summary=self._disabled_summary(),
            ).to_dict()
        summary = self.store.load_summary()
        relevant = self.find_relevant_files(goal, hints)
        context = self.get_context_candidates(goal, budget_tokens=budget_tokens, hints=hints)
        return build_project_index_observation(
            index_id=self.index_id,
            summary=summary,
            relevant_files=relevant,
            context_candidates=context,
        ).to_dict()

    def health_check(self) -> dict[str, Any]:
        if not self.config.enabled:
            return {
                "ok": True,
                "enabled": False,
                "db_path": str(self.db_path),
                "bootstrapped": self._bootstrapped,
                "summary": _bounded_summary(self._disabled_summary()),
            }
        summary = self.store.load_summary()
        return {
            "ok": self.config.enabled and self.db_path.exists(),
            "db_path": str(self.db_path),
            "bootstrapped": self._bootstrapped,
            "summary": _bounded_summary(summary),
        }

    def _disabled_summary(self) -> IndexSummary:
        return IndexSummary(limitations=["project_index_disabled"])

    def _index_paths(self, paths: Iterable[str]) -> list[str]:
        normalized = sorted(set(paths))
        if not normalized:
            return []
        files = self.scanner.scan_paths(normalized)
        if not files:
            return []
        for file in files:
            self.store.clear_file_facts(file.path)
        project_roots, entrypoints, config_facts, symbols, dependencies, references, call_edges, tests, docs = self._extract_facts_for_subset(files)
        dependencies = self._resolve_dependency_paths(dependencies, self.store.all_files() + files)
        self.store.upsert_files(files)
        self.store.upsert_project_roots(project_roots)
        self.store.upsert_entrypoints(entrypoints)
        self.store.upsert_config_facts(config_facts)
        self.store.upsert_symbols(symbols)
        self.store.upsert_dependencies(dependencies)
        self.store.upsert_references(references)
        self.store.upsert_call_edges(call_edges)
        self.store.upsert_test_mappings(tests)
        self.store.upsert_doc_sections(docs)
        return [file.path for file in files]

    def _extract_facts(
        self,
        files: list[FileRecord],
    ) -> tuple[
        list[ProjectRootRecord],
        list[EntryPointRecord],
        list[ConfigFactRecord],
        list[SymbolRecord],
        list[DependencyEdgeRecord],
        list[ReferenceRecord],
        list[CallEdgeRecord],
        list[TestMappingRecord],
        list[DocSectionRecord],
    ]:
        roots: list[ProjectRootRecord] = []
        for plugin in self.plugins:
            roots.extend(plugin.detect_project(self.workspace_root, files))
        return (roots, *self._extract_file_facts(files, files))

    def _extract_facts_for_subset(self, files: list[FileRecord]):
        all_files = self.store.all_files()
        roots: list[ProjectRootRecord] = []
        for plugin in self.plugins:
            roots.extend(plugin.detect_project(self.workspace_root, all_files + files))
        return (roots, *self._extract_file_facts(files, all_files + files))

    def _extract_file_facts(self, target_files: list[FileRecord], all_files: list[FileRecord]):
        entrypoints: list[EntryPointRecord] = []
        config_facts: list[ConfigFactRecord] = []
        symbols: list[SymbolRecord] = []
        dependencies: list[DependencyEdgeRecord] = []
        references: list[ReferenceRecord] = []
        call_edges: list[CallEdgeRecord] = []
        tests: list[TestMappingRecord] = []
        docs: list[DocSectionRecord] = []
        for file in target_files:
            if file.is_binary:
                continue
            for plugin in self._plugins_for(file):
                config_facts.extend(plugin.extract_config(self.workspace_root, file))
                entrypoints.extend(plugin.extract_entrypoints(self.workspace_root, file))
                file_symbols = plugin.extract_symbols(self.workspace_root, file)
                symbols.extend(file_symbols)
                dependencies.extend(plugin.extract_dependencies(self.workspace_root, file))
                references.extend(plugin.extract_references(self.workspace_root, file))
                call_edges.extend(plugin.extract_call_edges(self.workspace_root, file, file_symbols))
                tests.extend(plugin.extract_tests(self.workspace_root, file, all_files))
                docs.extend(plugin.summarize_doc(self.workspace_root, file))
        tests.extend(_conventional_test_mappings(all_files))
        return entrypoints, config_facts, symbols, dependencies, references, call_edges, _dedupe_tests(tests), docs

    def _plugins_for(self, file: FileRecord) -> list[LanguagePlugin]:
        matched = [plugin for plugin in self.plugins if plugin.supports(file)]
        if matched:
            return matched
        if any(str(role) == "doc" for role in file.roles):
            return [self.plugins[0]]
        return []

    def _resolve_dependency_paths(
        self,
        dependencies: list[DependencyEdgeRecord],
        files: list[FileRecord],
    ) -> list[DependencyEdgeRecord]:
        existing = {file.path for file in files}
        resolved: list[DependencyEdgeRecord] = []
        for edge in dependencies:
            path = edge.imported_path
            candidates = _dependency_candidates(edge.imported, edge.importer_path)
            if path not in existing:
                path = next((candidate for candidate in candidates if candidate in existing), path if path in existing else None)
            if path != edge.imported_path:
                edge = DependencyEdgeRecord(
                    importer_path=edge.importer_path,
                    imported=edge.imported,
                    imported_path=path,
                    kind=edge.kind,
                    line=edge.line,
                    optional=edge.optional,
                    freshness=edge.freshness,
                    confidence=edge.confidence if path else min(edge.confidence, 0.5),
                    evidence=edge.evidence,
                    trust_level=edge.trust_level,
                    backend=edge.backend,
                    source=edge.source,
                )
            resolved.append(edge)
        return resolved

    def _changed_and_deleted_from(self, changeset: Any) -> tuple[list[str], list[str]]:
        if isinstance(changeset, dict):
            changed = list(changeset.get("changed_files") or changeset.get("affected_files") or [])
            deleted = list(changeset.get("deleted_files") or [])
            return changed, deleted
        changed = list(getattr(changeset, "affected_files", None) or [])
        deleted = []
        final_texts = getattr(changeset, "final_texts", None)
        if isinstance(final_texts, dict):
            deleted = [path for path, text in final_texts.items() if text is None]
        return changed, deleted

    def _emit_index_event(self, event: str, payload: dict[str, Any]) -> None:
        event_type = {
            "project_index.updated": TraceEventType.PROJECT_INDEX_UPDATED,
            "project_index.refreshed": TraceEventType.PROJECT_INDEX_REFRESHED,
        }.get(event, TraceEventType.PROJECT_INDEX_UPDATED)
        self._emit(event_type, event, payload)

    def _emit(
        self,
        event_type: TraceEventType,
        summary: str,
        payload: dict[str, Any],
        *,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            component="project_index",
            summary=summary,
            payload=payload,
            severity=severity,
        )


def _bounded_summary(summary: IndexSummary) -> dict[str, Any]:
    return {
        "schema_version": summary.schema_version,
        "file_count": summary.file_count,
        "source_count": summary.source_count,
        "test_count": summary.test_count,
        "config_count": summary.config_count,
        "doc_count": summary.doc_count,
        "symbol_count": summary.symbol_count,
        "dependency_count": summary.dependency_count,
        "entrypoint_count": summary.entrypoint_count,
        "languages": summary.languages,
        "freshness": summary.freshness.value if hasattr(summary.freshness, "value") else str(summary.freshness),
    }


def _dependency_candidates(imported: str, importer_path: str) -> list[str]:
    candidates: list[str] = []
    clean = imported.strip(".")
    if clean:
        parts = clean.split(".")
        for index in range(len(parts), 0, -1):
            prefix = parts[:index]
            candidates.append("/".join(["src", *prefix]) + ".py")
            candidates.append("/".join(["src", *prefix, "__init__.py"]))
            candidates.append("/".join(prefix) + ".py")
            candidates.append("/".join([*prefix, "__init__.py"]))
            candidates.append("/".join(prefix) + ".ts")
            candidates.append("/".join(prefix) + ".js")
    if imported.startswith("."):
        base = PurePosixPath(importer_path).parent
        rel = imported.lstrip(".")
        if rel:
            candidates.append((base / rel).as_posix() + ".py")
            candidates.append((base / rel / "__init__.py").as_posix())
    return candidates


def _conventional_test_mappings(files: list[FileRecord]) -> list[TestMappingRecord]:
    existing = {file.path for file in files}
    mappings = []
    for file in files:
        if file.is_test:
            continue
        pure = PurePosixPath(file.path)
        candidates = []
        if pure.suffix == ".py":
            candidates.append(f"tests/test_{pure.stem}.py")
        elif pure.suffix in {".js", ".jsx", ".ts", ".tsx"}:
            candidates.append(f"{pure.with_suffix('').as_posix()}.test{pure.suffix}")
        for candidate in sorted(set(candidates) & existing):
            mappings.append(
                TestMappingRecord(
                    source_path=file.path,
                    test_path=candidate,
                    framework="convention",
                    reason="Cross-plugin conventional test mapping.",
                    confidence=0.55,
                    evidence=[Evidence(source="project_index", path=candidate)],
                    trust_level=TrustLevel.COMPONENT_GENERATED,
                    backend=BackendInfo(name="project_index", version=SCHEMA_VERSION),
                    source="project_index",
                )
            )
    return mappings


def _dedupe_tests(records: list[TestMappingRecord]) -> list[TestMappingRecord]:
    seen = set()
    deduped = []
    for record in sorted(records, key=lambda item: (item.source_path, item.test_path, -item.confidence)):
        key = (record.source_path, record.test_path, record.test_name)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(record)
    return deduped
