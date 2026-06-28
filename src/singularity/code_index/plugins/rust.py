from __future__ import annotations

import re
from collections.abc import Iterable
from pathlib import Path, PurePosixPath

from singularity.code_index.language import LanguagePlugin, safe_read_text
from singularity.code_index.models import (
    ConfigFactRecord,
    DependencyEdgeRecord,
    DependencyKind,
    EntryPointRecord,
    Evidence,
    FileRecord,
    LanguageId,
    ProjectKind,
    ProjectRootRecord,
    SymbolKind,
    SymbolRecord,
    TestMappingRecord,
    TrustLevel,
)

RUST_SYMBOL_RE = re.compile(r"^\s*(?:pub\s+)?(fn|struct|enum|trait|impl)\s+([A-Za-z_][A-Za-z0-9_]*)?", re.M)
RUST_USE_RE = re.compile(r"^\s*(?:pub\s+)?(?:use|mod)\s+([^;{]+)", re.M)


class RustPlugin(LanguagePlugin):
    name = "rust_static"
    version = "1.0.0"
    languages = ("rust",)

    def detect_project(
        self, workspace_root: Path, files: Iterable[FileRecord]
    ) -> list[ProjectRootRecord]:
        if not (workspace_root / "Cargo.toml").exists():
            return []
        text = safe_read_text(workspace_root, "Cargo.toml")
        return [
            ProjectRootRecord(
                root_path=".",
                kind=ProjectKind.MONOREPO if "[workspace]" in text else ProjectKind.SINGLE_PROJECT,
                languages=[LanguageId.RUST],
                package_manager="cargo",
                confidence=0.9,
                evidence=[Evidence(source=self.name, path="Cargo.toml")],
                backend=self.backend,
                source=self.name,
            )
        ]

    def extract_config(self, workspace_root: Path, file: FileRecord) -> list[ConfigFactRecord]:
        if file.path != "Cargo.toml":
            return []
        text = safe_read_text(workspace_root, file.path)
        facts = [
            ConfigFactRecord(
                path=file.path,
                key="rust.cargo_manifest",
                value=True,
                fact_type="project_config",
                language=LanguageId.RUST,
                confidence=0.9,
                evidence=[Evidence(source=self.name, path=file.path)],
                trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                backend=self.backend,
                source=self.name,
            )
        ]
        for match in re.finditer(r'^\s*name\s*=\s*"([^"]+)"', text, re.M):
            facts.append(
                ConfigFactRecord(
                    path=file.path,
                    key="rust.package.name",
                    value=match.group(1),
                    fact_type="package",
                    language=LanguageId.RUST,
                    confidence=0.8,
                    evidence=[Evidence(source=self.name, path=file.path, line_start=text.count("\n", 0, match.start()) + 1)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
            break
        return facts

    def extract_entrypoints(self, workspace_root: Path, file: FileRecord) -> list[EntryPointRecord]:
        if file.path not in {"src/main.rs", "src/lib.rs"}:
            return []
        return [
            EntryPointRecord(
                path=file.path,
                kind="rust_binary" if file.path.endswith("main.rs") else "rust_library",
                language=LanguageId.RUST,
                confidence=0.9,
                evidence=[Evidence(source=self.name, path=file.path)],
                backend=self.backend,
                source=self.name,
            )
        ]

    def extract_symbols(self, workspace_root: Path, file: FileRecord) -> list[SymbolRecord]:
        text = safe_read_text(workspace_root, file.path)
        records = []
        kind_map = {
            "fn": SymbolKind.FUNCTION,
            "struct": SymbolKind.STRUCT,
            "enum": SymbolKind.ENUM,
            "trait": SymbolKind.TRAIT,
            "impl": SymbolKind.IMPL,
        }
        for match in RUST_SYMBOL_RE.finditer(text):
            raw_kind, raw_name = match.group(1), match.group(2) or "impl"
            line = text.count("\n", 0, match.start()) + 1
            records.append(
                SymbolRecord(
                    path=file.path,
                    name=raw_name,
                    qualified_name=f"{file.path}:{raw_name}",
                    kind=kind_map.get(raw_kind, SymbolKind.UNKNOWN),
                    language=LanguageId.RUST,
                    line_start=line,
                    line_end=line,
                    exported=match.group(0).lstrip().startswith("pub "),
                    confidence=0.68,
                    evidence=[Evidence(source=self.name, path=file.path, line_start=line)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return records

    def extract_dependencies(self, workspace_root: Path, file: FileRecord) -> list[DependencyEdgeRecord]:
        text = safe_read_text(workspace_root, file.path)
        edges = []
        for match in RUST_USE_RE.finditer(text):
            imported = match.group(1).strip()
            line = text.count("\n", 0, match.start()) + 1
            edges.append(
                DependencyEdgeRecord(
                    importer_path=file.path,
                    imported=imported,
                    imported_path=_resolve_rust_module(file.path, imported),
                    kind=DependencyKind.USE if "use" in match.group(0) else DependencyKind.MOD,
                    line=line,
                    confidence=0.65,
                    evidence=[Evidence(source=self.name, path=file.path, line_start=line)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return edges

    def extract_tests(self, workspace_root: Path, file: FileRecord, files: Iterable[FileRecord]) -> list[TestMappingRecord]:
        text = safe_read_text(workspace_root, file.path)
        mappings = []
        if "#[test]" in text or PurePosixPath(file.path).parts[:1] == ("tests",):
            mappings.append(
                TestMappingRecord(
                    source_path=file.path,
                    test_path=file.path,
                    framework="cargo test",
                    reason="Rust test attribute or tests directory detected.",
                    confidence=0.86,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return mappings


def _resolve_rust_module(path: str, imported: str) -> str | None:
    module = imported.split("::")[0].strip()
    if not module or module in {"crate", "self", "super"}:
        return None
    base = PurePosixPath(path).parent
    return (base / f"{module}.rs").as_posix()
