from __future__ import annotations

from pathlib import PurePosixPath
from typing import Iterable

from singularity.code_index.models import (
    CodeImpactAnalysis,
    Evidence,
    FileRole,
    FreshnessStatus,
    TestImpactAnalysis,
    TrustLevel,
)
from singularity.code_index.store import ProjectIndexStore


class ProjectImpactAnalyzer:
    def __init__(self, store: ProjectIndexStore) -> None:
        self.store = store

    def analyze_paths(self, paths: Iterable[str]) -> CodeImpactAnalysis:
        normalized = sorted({str(path).replace("\\", "/") for path in paths if str(path)})
        files = self.store.files_by_path(normalized)
        direct = sorted(files)
        reverse_edges = self.store.query_reverse_dependencies(direct)
        reverse = sorted({edge.importer_path for edge in reverse_edges})
        symbols = self.store.symbols_for_paths([*direct, *reverse])
        tests = self.store.query_tests([*direct, *reverse])
        entrypoints = self.store.query_entrypoints()
        entrypoint_paths = {entry.path for entry in entrypoints}
        affected_entrypoints = sorted(entrypoint_paths & set([*direct, *reverse]))
        config_impact = any(FileRole.CONFIG in file.roles or FileRole.LOCKFILE in file.roles for file in files.values())
        generated_vendor = any(
            FileRole.GENERATED in file.roles or FileRole.VENDOR in file.roles or FileRole.BUILD_ARTIFACT in file.roles
            for file in files.values()
        )
        broad = config_impact or generated_vendor or len(reverse) > 8 or bool(affected_entrypoints)
        risk_reasons = []
        if config_impact:
            risk_reasons.append("Config or lockfile change can affect project-wide behavior.")
        if generated_vendor:
            risk_reasons.append("Generated, vendor, or build artifact path is in scope.")
        if reverse:
            risk_reasons.append("Reverse dependencies are affected.")
        if affected_entrypoints:
            risk_reasons.append("Entrypoint files are affected.")
        if len(normalized) > 10:
            risk_reasons.append("Large changed file set.")
        risk_level = "high" if broad else "medium" if reverse or symbols else "low"
        recommended = ["run targeted tests"]
        if broad:
            recommended.append("run full project verification")
        if config_impact:
            recommended.append("run build/typecheck")
        freshness = _combined_freshness([file.freshness for file in files.values()])
        return CodeImpactAnalysis(
            requested_paths=normalized,
            direct_files=direct,
            reverse_dependencies=reverse,
            affected_symbols=[symbol.qualified_name for symbol in symbols[:50]],
            affected_entrypoints=affected_entrypoints,
            affected_tests=sorted({mapping.test_path for mapping in tests}),
            config_impact=config_impact,
            generated_or_vendor_impact=generated_vendor,
            broad_impact=broad,
            risk_level=risk_level,
            risk_reasons=risk_reasons or ["No elevated code-index impact detected."],
            recommended_validation=recommended,
            freshness=freshness,
            confidence=0.82 if files else 0.45,
            evidence=[Evidence(source="project_index_impact", description="Impact from direct files, reverse dependencies, symbols, entrypoints, and tests.")],
            trust_level=TrustLevel.COMPONENT_GENERATED,
            source="project_index_impact",
        )

    def analyze_symbols(self, symbol_ids: Iterable[str]) -> CodeImpactAnalysis:
        ids = set(symbol_ids)
        symbols = [symbol for symbol in self.store.query_symbols("", limit=5000) if symbol.symbol_id in ids]
        return self.analyze_paths(symbol.path for symbol in symbols)

    def assess_mutation_scope(self, planned_files: Iterable[str]) -> CodeImpactAnalysis:
        return self.analyze_paths(planned_files)

    def get_test_impact(self, changed_files: Iterable[str]) -> TestImpactAnalysis:
        normalized = sorted({str(path).replace("\\", "/") for path in changed_files if str(path)})
        analysis = self.analyze_paths(normalized)
        likely = sorted(set(analysis.affected_tests) | set(_fallback_tests(normalized)))
        commands = []
        if likely:
            py_tests = [path for path in likely if path.endswith(".py")]
            if py_tests:
                commands.append("python -m pytest " + " ".join(py_tests))
            if any(path.endswith((".js", ".jsx", ".ts", ".tsx")) for path in likely):
                commands.append("npm test")
            if any(path.endswith(".rs") for path in normalized):
                commands.append("cargo test")
        if analysis.broad_impact:
            commands.append("full project verification")
        return TestImpactAnalysis(
            changed_files=normalized,
            likely_tests=likely,
            commands=commands,
            require_full_test=analysis.broad_impact,
            confidence_note="Widen verification if mappings are stale or low confidence.",
            freshness=analysis.freshness,
            confidence=analysis.confidence,
            evidence=analysis.evidence,
            trust_level=TrustLevel.COMPONENT_GENERATED,
            source="project_index_impact",
        )


def _fallback_tests(paths: Iterable[str]) -> list[str]:
    tests = []
    for path in paths:
        pure = PurePosixPath(path)
        if "tests" in pure.parts or pure.name.startswith("test_"):
            tests.append(path)
        elif pure.suffix == ".py":
            tests.append(f"tests/test_{pure.stem}.py")
        elif pure.suffix in {".js", ".jsx", ".ts", ".tsx"}:
            tests.append(f"{pure.with_suffix('').as_posix()}.test{pure.suffix}")
    return tests


def _combined_freshness(values) -> FreshnessStatus:
    values = list(values)
    if not values:
        return FreshnessStatus.UNKNOWN
    if any(value == FreshnessStatus.INVALID for value in values):
        return FreshnessStatus.INVALID
    if any(value != FreshnessStatus.FRESH for value in values):
        return FreshnessStatus.STALE_CONTENT
    return FreshnessStatus.FRESH
