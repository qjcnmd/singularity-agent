from __future__ import annotations

import json
import re
from pathlib import Path, PurePosixPath
from typing import Iterable

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


IMPORT_RE = re.compile(r"""(?:import\s+(?:.+?\s+from\s+)?|export\s+.+?\s+from\s+|require\()\s*['"]([^'"]+)['"]""")
FUNCTION_RE = re.compile(r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(", re.M)
CLASS_RE = re.compile(r"^\s*(?:export\s+)?class\s+([A-Za-z_$][\w$]*)", re.M)
CONST_FUNCTION_RE = re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?\(", re.M)


class JavaScriptPlugin(LanguagePlugin):
    name = "javascript_static"
    version = "1.0.0"
    languages = ("javascript",)
    language = LanguageId.JAVASCRIPT

    def detect_project(
        self, workspace_root: Path, files: Iterable[FileRecord]
    ) -> list[ProjectRootRecord]:
        paths = {file.path for file in files}
        if "package.json" not in paths:
            return []
        package = _package_json(workspace_root)
        return [
            ProjectRootRecord(
                root_path=".",
                kind=ProjectKind.MONOREPO if package.get("workspaces") else ProjectKind.SINGLE_PROJECT,
                languages=[self.language],
                package_manager=_node_package_manager(workspace_root),
                framework=_framework(package),
                confidence=0.9,
                evidence=[Evidence(source=self.name, path="package.json")],
                backend=self.backend,
                source=self.name,
            )
        ]

    def extract_config(self, workspace_root: Path, file: FileRecord) -> list[ConfigFactRecord]:
        if file.path != "package.json":
            return []
        package = _package_json(workspace_root)
        facts = []
        for key, value in (package.get("scripts") or {}).items():
            facts.append(
                ConfigFactRecord(
                    path=file.path,
                    key=f"package.scripts.{key}",
                    value=value,
                    fact_type="script",
                    language=self.language,
                    confidence=0.88,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return facts

    def extract_entrypoints(
        self, workspace_root: Path, file: FileRecord
    ) -> list[EntryPointRecord]:
        package = _package_json(workspace_root)
        entries = []
        for key in ("main", "module", "bin"):
            value = package.get(key)
            if isinstance(value, str) and _normalize(value) == file.path:
                entries.append(
                    EntryPointRecord(
                        path=file.path,
                        kind=f"package_{key}",
                        language=self.language,
                        confidence=0.9,
                        evidence=[Evidence(source=self.name, path="package.json")],
                        backend=self.backend,
                        source=self.name,
                    )
                )
        if PurePosixPath(file.path).name in {"index.js", "main.js", "server.js"}:
            entries.append(
                EntryPointRecord(
                    path=file.path,
                    kind="conventional_entrypoint",
                    language=self.language,
                    confidence=0.65,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    backend=self.backend,
                    source=self.name,
                )
            )
        return entries

    def extract_symbols(self, workspace_root: Path, file: FileRecord) -> list[SymbolRecord]:
        text = safe_read_text(workspace_root, file.path)
        records = []
        for regex, kind in ((CLASS_RE, SymbolKind.CLASS), (FUNCTION_RE, SymbolKind.FUNCTION), (CONST_FUNCTION_RE, SymbolKind.FUNCTION)):
            for match in regex.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                name = match.group(1)
                records.append(
                    SymbolRecord(
                        path=file.path,
                        name=name,
                        qualified_name=f"{file.path}:{name}",
                        kind=kind,
                        language=self.language,
                        line_start=line,
                        line_end=line,
                        exported="export" in match.group(0),
                        confidence=0.72,
                        evidence=[Evidence(source=self.name, path=file.path, line_start=line)],
                        backend=self.backend,
                        source=self.name,
                    )
                )
        return records

    def extract_dependencies(
        self, workspace_root: Path, file: FileRecord
    ) -> list[DependencyEdgeRecord]:
        text = safe_read_text(workspace_root, file.path)
        return [
            DependencyEdgeRecord(
                importer_path=file.path,
                imported=match.group(1),
                imported_path=_resolve_import(file.path, match.group(1)),
                kind=DependencyKind.IMPORT,
                line=text.count("\n", 0, match.start()) + 1,
                confidence=0.68,
                evidence=[Evidence(source=self.name, path=file.path, line_start=text.count("\n", 0, match.start()) + 1)],
                trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                backend=self.backend,
                source=self.name,
            )
            for match in IMPORT_RE.finditer(text)
        ]

    def extract_tests(
        self, workspace_root: Path, file: FileRecord, files: Iterable[FileRecord]
    ) -> list[TestMappingRecord]:
        if file.language != self.language:
            return []
        if file.is_test:
            return [
                TestMappingRecord(
                    source_path=file.path,
                    test_path=file.path,
                    framework="node-test",
                    reason="File matches JS test convention.",
                    confidence=0.9,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    backend=self.backend,
                    source=self.name,
                )
            ]
        existing = {item.path for item in files}
        pure = PurePosixPath(file.path)
        stem = pure.with_suffix("").as_posix()
        candidates = {f"{stem}.test{pure.suffix}", f"{stem}.spec{pure.suffix}"}
        return [
            TestMappingRecord(
                source_path=file.path,
                test_path=candidate,
                framework="node-test",
                reason="Conventional JS source/test file naming.",
                confidence=0.7,
                evidence=[Evidence(source=self.name, path=candidate)],
                backend=self.backend,
                source=self.name,
            )
            for candidate in sorted(candidates & existing)
        ]


def _package_json(workspace_root: Path) -> dict:
    path = workspace_root / "package.json"
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


def _node_package_manager(workspace_root: Path) -> str:
    for name, manager in (("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("package-lock.json", "npm")):
        if (workspace_root / name).exists():
            return manager
    return "npm"


def _framework(package: dict) -> str | None:
    deps = {}
    for key in ("dependencies", "devDependencies"):
        deps.update(package.get(key) or {})
    for framework in ("next", "vite", "react", "vue", "svelte", "express"):
        if framework in deps:
            return framework
    return None


def _resolve_import(path: str, imported: str) -> str | None:
    if not imported.startswith("."):
        return None
    base = PurePosixPath(path).parent
    target = (base / imported).as_posix()
    if PurePosixPath(target).suffix:
        return target
    return f"{target}.js"


def _normalize(value: str) -> str:
    return value.replace("\\", "/").lstrip("./")
