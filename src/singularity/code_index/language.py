from __future__ import annotations

from collections.abc import Iterable
from pathlib import Path

from singularity.code_index.models import (
    BackendInfo,
    CallEdgeRecord,
    ConfigFactRecord,
    DependencyEdgeRecord,
    DocSectionRecord,
    EntryPointRecord,
    FileRecord,
    ProjectRootRecord,
    ReferenceRecord,
    SymbolRecord,
    TestMappingRecord,
)


class LanguagePlugin:
    name = "base"
    version = "1.0.0"
    languages: tuple[str, ...] = ()

    @property
    def backend(self) -> BackendInfo:
        return BackendInfo(name=self.name, version=self.version, source="language_plugin")

    def supports(self, file: FileRecord) -> bool:
        return file.language.value in self.languages

    def detect_project(
        self, workspace_root: Path, files: Iterable[FileRecord]
    ) -> list[ProjectRootRecord]:
        return []

    def classify_file(self, file: FileRecord) -> FileRecord:
        return file

    def extract_config(self, workspace_root: Path, file: FileRecord) -> list[ConfigFactRecord]:
        return []

    def extract_entrypoints(
        self, workspace_root: Path, file: FileRecord
    ) -> list[EntryPointRecord]:
        return []

    def extract_symbols(self, workspace_root: Path, file: FileRecord) -> list[SymbolRecord]:
        return []

    def extract_dependencies(
        self, workspace_root: Path, file: FileRecord
    ) -> list[DependencyEdgeRecord]:
        return []

    def extract_references(
        self, workspace_root: Path, file: FileRecord
    ) -> list[ReferenceRecord]:
        return []

    def extract_call_edges(
        self, workspace_root: Path, file: FileRecord, symbols: list[SymbolRecord]
    ) -> list[CallEdgeRecord]:
        return []

    def extract_tests(
        self, workspace_root: Path, file: FileRecord, files: Iterable[FileRecord]
    ) -> list[TestMappingRecord]:
        return []

    def summarize_doc(self, workspace_root: Path, file: FileRecord) -> list[DocSectionRecord]:
        return []


def safe_read_text(
    workspace_root: Path,
    relative_path: str,
    *,
    max_bytes: int = 500_000,
) -> str:
    root = workspace_root.resolve(strict=False)
    candidate = (root / relative_path).resolve(strict=False)
    try:
        candidate.relative_to(root)
    except ValueError:
        return ""
    try:
        if candidate.stat().st_size > max_bytes:
            return ""
        return candidate.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return ""
