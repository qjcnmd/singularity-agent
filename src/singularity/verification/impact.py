from __future__ import annotations

from pathlib import PurePosixPath

from singularity.verification.models import ImpactAnalysis, ProjectProfile

DOC_SUFFIXES = {".md", ".mdx", ".rst", ".txt", ".adoc"}
SOURCE_SUFFIXES = {".py", ".js", ".jsx", ".ts", ".tsx", ".rs", ".go", ".java"}
CONFIG_NAMES = {
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "setup.py",
    "setup.cfg",
    "Cargo.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "Makefile",
    "justfile",
    "tsconfig.json",
    ".eslintrc",
    ".eslintrc.json",
    "eslint.config.js",
}
LOCK_NAMES = {
    "pnpm-lock.yaml",
    "yarn.lock",
    "package-lock.json",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
}


class ImpactAnalyzer:
    def analyze(
        self,
        *,
        changed_files: list[str],
        task_intent: str,
        project_profile: ProjectProfile,
        transaction_id: str | None = None,
        changeset_id: str | None = None,
    ) -> ImpactAnalysis:
        normalized = sorted({path.replace("\\", "/") for path in changed_files})
        risk_reasons: list[str] = []
        affected_modules = sorted({self._module_for(path) for path in normalized})
        likely_tests = self._likely_tests(normalized)
        docs_only = bool(normalized) and all(self._is_doc(path) for path in normalized)
        source_changed = any(self._is_source(path) for path in normalized)
        tests_changed = any(self._is_test(path) for path in normalized)
        config_changed = any(self._is_config(path) for path in normalized)
        lock_changed = any(PurePosixPath(path).name in LOCK_NAMES for path in normalized)
        ci_changed = any(path.startswith(".github/workflows/") for path in normalized)
        deploy_changed = any(
            "docker" in path.lower()
            or "deploy" in path.lower()
            or PurePosixPath(path).name.lower() in {"dockerfile", "compose.yaml", "docker-compose.yml"}
            for path in normalized
        )
        public_api_changed = any(
            path.endswith("__init__.py") or "/api/" in f"/{path}/" or path.startswith("src/")
            for path in normalized
        )

        if not normalized:
            risk_reasons.append("No changed files were provided; verification scope is conservative.")
        if docs_only:
            risk_reasons.append("Only documentation-like files changed.")
        if source_changed:
            risk_reasons.append("Source files changed.")
        if tests_changed:
            risk_reasons.append("Test files changed.")
        if config_changed:
            risk_reasons.append("Project configuration changed.")
        if lock_changed:
            risk_reasons.append("Dependency lockfile changed.")
        if ci_changed:
            risk_reasons.append("CI workflow changed.")
        if deploy_changed:
            risk_reasons.append("Deployment or container configuration changed.")
        if public_api_changed:
            risk_reasons.append("Public package/API surface may have changed.")

        requires_manual_review = ci_changed or deploy_changed or lock_changed
        requires_full_test = (
            not docs_only
            and (config_changed or lock_changed or ci_changed or public_api_changed or len(normalized) > 5)
        )
        requires_build = not docs_only and (
            config_changed or lock_changed or deploy_changed or public_api_changed
        )
        requires_typecheck = not docs_only and (
            source_changed or config_changed or bool(project_profile.typecheck_tools)
        )

        if docs_only:
            risk_level = "low"
        elif requires_manual_review or requires_full_test or requires_build:
            risk_level = "high"
        elif source_changed or tests_changed:
            risk_level = "medium"
        else:
            risk_level = "medium"

        return ImpactAnalysis(
            changed_files=normalized,
            affected_modules=affected_modules,
            likely_tests=likely_tests,
            requires_full_test=requires_full_test,
            requires_build=requires_build,
            requires_typecheck=requires_typecheck,
            requires_manual_review=requires_manual_review,
            risk_reasons=risk_reasons,
            risk_level=risk_level,
            transaction_id=transaction_id,
            changeset_id=changeset_id,
        )

    @staticmethod
    def _module_for(path: str) -> str:
        parts = PurePosixPath(path).parts
        if not parts:
            return "."
        if parts[0] in {"src", "tests"} and len(parts) > 1:
            return "/".join(parts[:2])
        return parts[0]

    @staticmethod
    def _likely_tests(paths: list[str]) -> list[str]:
        tests = []
        for path in paths:
            pure = PurePosixPath(path)
            if "tests" in pure.parts or pure.name.startswith("test_"):
                tests.append(path)
            elif pure.suffix == ".py" and path.startswith("src/"):
                tests.append(f"tests/test_{pure.stem}.py")
            elif pure.suffix in {".ts", ".tsx", ".js", ".jsx"}:
                tests.append(f"{pure.with_suffix('').as_posix()}.test{pure.suffix}")
        return sorted(set(tests))

    @staticmethod
    def _is_doc(path: str) -> bool:
        pure = PurePosixPath(path)
        return pure.suffix.lower() in DOC_SUFFIXES or "docs" in pure.parts

    @staticmethod
    def _is_source(path: str) -> bool:
        return PurePosixPath(path).suffix.lower() in SOURCE_SUFFIXES

    @staticmethod
    def _is_test(path: str) -> bool:
        pure = PurePosixPath(path)
        return "tests" in pure.parts or pure.name.startswith("test_") or ".test." in pure.name

    @staticmethod
    def _is_config(path: str) -> bool:
        return PurePosixPath(path).name in CONFIG_NAMES
