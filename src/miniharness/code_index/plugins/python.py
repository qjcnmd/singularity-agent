from __future__ import annotations

import ast
import re
from pathlib import Path, PurePosixPath
from typing import Iterable

from miniharness.code_index.language import LanguagePlugin, safe_read_text
from miniharness.code_index.models import (
    ConfigFactRecord,
    DependencyEdgeRecord,
    DependencyKind,
    DocSectionRecord,
    EntryPointRecord,
    Evidence,
    FileRecord,
    FileRole,
    LanguageId,
    ProjectKind,
    ProjectRootRecord,
    SymbolKind,
    SymbolRecord,
    TestMappingRecord,
    TrustLevel,
)


class PythonPlugin(LanguagePlugin):
    name = "python_ast"
    version = "1.0.0"
    languages = ("python",)

    def detect_project(
        self, workspace_root: Path, files: Iterable[FileRecord]
    ) -> list[ProjectRootRecord]:
        paths = {file.path for file in files}
        if not ({"pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"} & paths):
            if not any(file.language == LanguageId.PYTHON for file in files):
                return []
        return [
            ProjectRootRecord(
                root_path=".",
                kind=ProjectKind.SINGLE_PROJECT,
                languages=[LanguageId.PYTHON],
                package_manager="python",
                framework=_python_framework(workspace_root),
                confidence=0.9,
                evidence=[Evidence(source=self.name, description="Python files or Python config detected.")],
                backend=self.backend,
                source=self.name,
            )
        ]

    def extract_config(self, workspace_root: Path, file: FileRecord) -> list[ConfigFactRecord]:
        if PurePosixPath(file.path).name not in {"pyproject.toml", "setup.cfg", "setup.py", "requirements.txt"}:
            return []
        text = safe_read_text(workspace_root, file.path, max_bytes=300_000)
        facts = [
            ConfigFactRecord(
                path=file.path,
                key="python.config_file",
                value=PurePosixPath(file.path).name,
                fact_type="project_config",
                language=LanguageId.PYTHON,
                confidence=0.8,
                evidence=[Evidence(source=self.name, path=file.path)],
                trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                backend=self.backend,
                source=self.name,
            )
        ]
        for marker, key in (
            ("pytest", "test_framework"),
            ("ruff", "lint_tool"),
            ("mypy", "typecheck_tool"),
            ("typer", "cli_framework"),
            ("click", "cli_framework"),
            ("fastapi", "web_framework"),
        ):
            if marker.lower() in text.lower():
                facts.append(
                    ConfigFactRecord(
                        path=file.path,
                        key=f"python.{key}",
                        value=marker,
                        fact_type=key,
                        language=LanguageId.PYTHON,
                        confidence=0.65,
                        evidence=[Evidence(source=self.name, path=file.path, description=f"{marker} mentioned.")],
                        trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                        backend=self.backend,
                        source=self.name,
                    )
                )
        return facts

    def extract_entrypoints(
        self, workspace_root: Path, file: FileRecord
    ) -> list[EntryPointRecord]:
        path = PurePosixPath(file.path)
        text = safe_read_text(workspace_root, file.path)
        entries: list[EntryPointRecord] = []
        if path.name in {"cli.py", "__main__.py", "main.py", "app.py"}:
            entries.append(
                EntryPointRecord(
                    path=file.path,
                    kind="python_module",
                    language=LanguageId.PYTHON,
                    confidence=0.8,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    backend=self.backend,
                    source=self.name,
                )
            )
        if "typer.Typer(" in text or "click.group(" in text or "FastAPI(" in text:
            entries.append(
                EntryPointRecord(
                    path=file.path,
                    kind="framework_entrypoint",
                    symbol=_first_app_symbol(text),
                    language=LanguageId.PYTHON,
                    confidence=0.78,
                    evidence=[Evidence(source=self.name, path=file.path, description="Typer/Click/FastAPI pattern detected.")],
                    backend=self.backend,
                    source=self.name,
                )
            )
        return entries

    def extract_symbols(self, workspace_root: Path, file: FileRecord) -> list[SymbolRecord]:
        tree = _parse(workspace_root, file.path)
        if tree is None:
            return []
        symbols: list[SymbolRecord] = []
        module_name = _module_name(file.path)
        symbols.append(
            SymbolRecord(
                path=file.path,
                name=module_name,
                qualified_name=module_name,
                kind=SymbolKind.MODULE,
                language=LanguageId.PYTHON,
                line_start=1,
                line_end=getattr(tree, "end_lineno", None),
                confidence=0.95,
                evidence=[Evidence(source=self.name, path=file.path, line_start=1)],
                backend=self.backend,
                source=self.name,
            )
        )
        parents: list[str] = []
        for node in ast.walk(tree):
            if isinstance(node, ast.ClassDef):
                qualified = ".".join([module_name, *parents, node.name])
                symbols.append(
                    _symbol(file.path, node.name, qualified, SymbolKind.CLASS, node, self.backend.name, self.backend.version)
                )
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                kind = SymbolKind.TEST if node.name.startswith("test_") or file.is_test else SymbolKind.FUNCTION
                qualified = f"{module_name}.{node.name}"
                symbols.append(
                    _symbol(file.path, node.name, qualified, kind, node, self.backend.name, self.backend.version)
                )
        return symbols

    def extract_dependencies(
        self, workspace_root: Path, file: FileRecord
    ) -> list[DependencyEdgeRecord]:
        tree = _parse(workspace_root, file.path)
        if tree is None:
            return []
        edges: list[DependencyEdgeRecord] = []
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for alias in node.names:
                    edges.append(self._dependency(file.path, alias.name, getattr(node, "lineno", None)))
            elif isinstance(node, ast.ImportFrom):
                module = "." * int(node.level or 0) + (node.module or "")
                for alias in node.names:
                    imported = f"{module}.{alias.name}".strip(".")
                    edges.append(self._dependency(file.path, imported or module, getattr(node, "lineno", None)))
        return edges

    def extract_call_edges(
        self, workspace_root: Path, file: FileRecord, symbols: list[SymbolRecord]
    ) -> list:
        tree = _parse(workspace_root, file.path)
        if tree is None:
            return []
        from miniharness.code_index.models import CallEdgeRecord

        by_line = sorted(
            [symbol for symbol in symbols if symbol.path == file.path and symbol.line_start],
            key=lambda symbol: symbol.line_start or 0,
        )
        edges: list[CallEdgeRecord] = []
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            caller = _enclosing_symbol(by_line, getattr(node, "lineno", 0))
            if caller is None:
                continue
            callee = _call_name(node.func)
            if not callee:
                continue
            edges.append(
                CallEdgeRecord(
                    caller_symbol_id=caller.symbol_id,
                    callee=callee,
                    path=file.path,
                    line=getattr(node, "lineno", None),
                    confidence=0.55,
                    evidence=[Evidence(source=self.name, path=file.path, line_start=getattr(node, "lineno", None))],
                    backend=self.backend,
                    source=self.name,
                )
            )
        return edges

    def extract_tests(
        self, workspace_root: Path, file: FileRecord, files: Iterable[FileRecord]
    ) -> list[TestMappingRecord]:
        all_files = list(files)
        if file.language != LanguageId.PYTHON:
            return []
        if file.is_test:
            return [
                TestMappingRecord(
                    source_path=file.path,
                    test_path=file.path,
                    framework="pytest",
                    reason="File is a pytest test file.",
                    confidence=0.95,
                    evidence=[Evidence(source=self.name, path=file.path)],
                    backend=self.backend,
                    source=self.name,
                )
            ]
        pure = PurePosixPath(file.path)
        candidates = {
            f"tests/test_{pure.stem}.py",
            f"tests/{pure.stem}_test.py",
            pure.with_name(f"test_{pure.name}").as_posix(),
        }
        mappings = []
        existing = {candidate.path for candidate in all_files}
        for candidate in sorted(candidates & existing):
            mappings.append(
                TestMappingRecord(
                    source_path=file.path,
                    test_path=candidate,
                    framework="pytest",
                    reason="Conventional pytest source/test file naming.",
                    confidence=0.72,
                    evidence=[Evidence(source=self.name, path=candidate)],
                    backend=self.backend,
                    source=self.name,
                )
            )
        return mappings

    def summarize_doc(self, workspace_root: Path, file: FileRecord) -> list[DocSectionRecord]:
        if FileRole.DOC not in file.roles:
            return []
        sections: list[DocSectionRecord] = []
        lines = safe_read_text(workspace_root, file.path, max_bytes=300_000).splitlines()
        headings: list[tuple[int, int, str]] = []
        for index, line in enumerate(lines, start=1):
            match = re.match(r"^(#{1,6})\s+(.+?)\s*$", line)
            if match:
                headings.append((index, len(match.group(1)), match.group(2)))
        for offset, (line, level, title) in enumerate(headings):
            next_line = headings[offset + 1][0] - 1 if offset + 1 < len(headings) else len(lines)
            sections.append(
                DocSectionRecord(
                    path=file.path,
                    title=title[:160],
                    level=level,
                    line_start=line,
                    line_end=next_line,
                    anchor=re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-"),
                    summary="\n".join(lines[line : min(next_line, line + 4)])[:600],
                    confidence=0.7,
                    evidence=[Evidence(source=self.name, path=file.path, line_start=line)],
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    backend=self.backend,
                    source=self.name,
                )
            )
        return sections

    def _dependency(self, path: str, imported: str, line: int | None) -> DependencyEdgeRecord:
        return DependencyEdgeRecord(
            importer_path=path,
            imported=imported,
            imported_path=_resolve_python_import(path, imported),
            kind=DependencyKind.IMPORT,
            line=line,
            confidence=0.75,
            evidence=[Evidence(source=self.name, path=path, line_start=line)],
            backend=self.backend,
            source=self.name,
        )


def _parse(workspace_root: Path, relative_path: str) -> ast.AST | None:
    text = safe_read_text(workspace_root, relative_path)
    if not text:
        return None
    try:
        return ast.parse(text, filename=relative_path)
    except SyntaxError:
        return None


def _module_name(path: str) -> str:
    pure = PurePosixPath(path)
    parts = list(pure.with_suffix("").parts)
    if parts and parts[0] == "src":
        parts = parts[1:]
    if parts[-1:] == ["__init__"]:
        parts = parts[:-1]
    return ".".join(parts) or pure.stem


def _symbol(path: str, name: str, qualified: str, kind: SymbolKind, node: ast.AST, backend_name: str, backend_version: str) -> SymbolRecord:
    from miniharness.code_index.models import BackendInfo

    return SymbolRecord(
        path=path,
        name=name,
        qualified_name=qualified,
        kind=kind,
        language=LanguageId.PYTHON,
        line_start=getattr(node, "lineno", None),
        line_end=getattr(node, "end_lineno", None),
        signature=_signature(node),
        exported=not name.startswith("_"),
        confidence=0.92,
        evidence=[Evidence(source=backend_name, path=path, line_start=getattr(node, "lineno", None), line_end=getattr(node, "end_lineno", None))],
        backend=BackendInfo(name=backend_name, version=backend_version, source="language_plugin"),
        source=backend_name,
    )


def _signature(node: ast.AST) -> str | None:
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        args = [arg.arg for arg in node.args.args]
        return f"{node.name}({', '.join(args)})"
    if isinstance(node, ast.ClassDef):
        bases = [_call_name(base) or "base" for base in node.bases]
        return f"class {node.name}({', '.join(bases)})" if bases else f"class {node.name}"
    return None


def _call_name(node: ast.AST) -> str | None:
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        prefix = _call_name(node.value)
        return f"{prefix}.{node.attr}" if prefix else node.attr
    return None


def _enclosing_symbol(symbols: list[SymbolRecord], line: int) -> SymbolRecord | None:
    candidates = [
        symbol
        for symbol in symbols
        if symbol.line_start is not None
        and symbol.line_end is not None
        and symbol.line_start <= line <= symbol.line_end
        and symbol.kind in {SymbolKind.FUNCTION, SymbolKind.METHOD, SymbolKind.TEST}
    ]
    if not candidates:
        return None
    return sorted(candidates, key=lambda symbol: symbol.line_start or 0, reverse=True)[0]


def _resolve_python_import(path: str, imported: str) -> str | None:
    name = imported.strip(".").split(".")[0]
    if not name:
        return None
    parts = PurePosixPath(path).parts
    if parts and parts[0] == "src":
        return f"src/{name}.py"
    return f"{name}.py"


def _first_app_symbol(text: str) -> str | None:
    match = re.search(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(typer\.Typer|FastAPI)\(", text, re.M)
    return match.group(1) if match else None


def _python_framework(workspace_root: Path) -> str | None:
    text = ""
    for path in ("pyproject.toml", "requirements.txt"):
        candidate = workspace_root / path
        if candidate.exists():
            text += candidate.read_text(encoding="utf-8", errors="ignore").lower()
    for framework in ("fastapi", "django", "flask", "typer", "click"):
        if framework in text:
            return framework
    return None
