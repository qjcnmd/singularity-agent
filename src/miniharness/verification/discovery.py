from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path
from typing import Any

from miniharness.command import (
    CommandPurpose,
    CommandRequest,
    FilesystemMode,
)
from miniharness.verification.models import (
    CheckKind,
    DiscoveredCommand,
    ProjectLanguage,
    ProjectProfile,
    WorkspaceKind,
)


PACKAGE_LOCKS = {
    "pnpm-lock.yaml": "pnpm",
    "yarn.lock": "yarn",
    "package-lock.json": "npm",
}


class ProjectDetector:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)

    def detect(self) -> ProjectProfile:
        evidence = self._evidence_files()
        commands = CommandDiscovery(self.workspace_root).discover()
        languages = self._languages(evidence)
        package_manager = self._package_manager(evidence)
        package_json = self._package_json()
        pyproject = self._pyproject()
        dependencies = {
            *self._package_dependencies(package_json),
            *self._python_dependencies(pyproject),
        }
        test_frameworks = sorted(self._test_frameworks(evidence, dependencies, commands))
        lint_tools = sorted(self._lint_tools(evidence, dependencies, commands))
        typecheck_tools = sorted(self._typecheck_tools(evidence, dependencies, commands))
        build_tools = sorted(self._build_tools(evidence, commands))
        framework = self._framework(dependencies)
        workspace_kind = self._workspace_kind(evidence, package_json, pyproject)
        primary = self._primary_language(languages)
        return ProjectProfile(
            languages=languages or [ProjectLanguage.UNKNOWN],
            language=primary,
            package_manager=package_manager,
            framework=framework,
            test_frameworks=test_frameworks,
            lint_tools=lint_tools,
            typecheck_tools=typecheck_tools,
            build_tools=build_tools,
            workspace_kind=workspace_kind,
            available_commands=commands,
            evidence_files=evidence,
        )

    def _evidence_files(self) -> list[str]:
        names = {
            "package.json",
            "pnpm-lock.yaml",
            "yarn.lock",
            "package-lock.json",
            "pnpm-workspace.yaml",
            "turbo.json",
            "nx.json",
            "pyproject.toml",
            "requirements.txt",
            "setup.py",
            "setup.cfg",
            "tox.ini",
            "pytest.ini",
            "ruff.toml",
            ".ruff.toml",
            "mypy.ini",
            "Cargo.toml",
            "Cargo.lock",
            "go.mod",
            "go.work",
            "pom.xml",
            "build.gradle",
            "settings.gradle",
            "Makefile",
            "justfile",
            "tsconfig.json",
            ".eslintrc",
            ".eslintrc.json",
            ".eslintrc.js",
            "eslint.config.js",
        }
        evidence = [
            path.relative_to(self.workspace_root).as_posix()
            for path in self.workspace_root.rglob("*")
            if path.is_file()
            and (
                path.name in names
                or path.relative_to(self.workspace_root).as_posix().startswith(
                    ".github/workflows/"
                )
            )
        ]
        return sorted(evidence)

    def _languages(self, evidence: list[str]) -> list[ProjectLanguage]:
        languages: set[ProjectLanguage] = set()
        if any(path in {"pyproject.toml", "requirements.txt", "setup.py"} for path in evidence):
            languages.add(ProjectLanguage.PYTHON)
        if "package.json" in evidence:
            languages.add(
                ProjectLanguage.TYPESCRIPT
                if "tsconfig.json" in evidence
                else ProjectLanguage.JAVASCRIPT
            )
        if "Cargo.toml" in evidence:
            languages.add(ProjectLanguage.RUST)
        if "go.mod" in evidence:
            languages.add(ProjectLanguage.GO)
        if any(path in {"pom.xml", "build.gradle"} for path in evidence):
            languages.add(ProjectLanguage.JAVA)
        return sorted(languages, key=lambda language: language.value)

    def _primary_language(self, languages: list[ProjectLanguage]) -> ProjectLanguage:
        for candidate in (
            ProjectLanguage.PYTHON,
            ProjectLanguage.TYPESCRIPT,
            ProjectLanguage.JAVASCRIPT,
            ProjectLanguage.RUST,
            ProjectLanguage.GO,
            ProjectLanguage.JAVA,
        ):
            if candidate in languages:
                return candidate
        return ProjectLanguage.UNKNOWN

    def _package_manager(self, evidence: list[str]) -> str | None:
        for file_name, manager in PACKAGE_LOCKS.items():
            if file_name in evidence:
                return manager
        if "package.json" in evidence:
            return "npm"
        if (self.workspace_root / "uv.lock").exists():
            return "uv"
        if (self.workspace_root / "poetry.lock").exists():
            return "poetry"
        if "pyproject.toml" in evidence or "requirements.txt" in evidence:
            return "pip"
        if "Cargo.toml" in evidence:
            return "cargo"
        if "go.mod" in evidence:
            return "go"
        if "pom.xml" in evidence:
            return "maven"
        if "build.gradle" in evidence:
            return "gradle"
        return None

    def _workspace_kind(
        self,
        evidence: list[str],
        package_json: dict[str, Any],
        pyproject: dict[str, Any],
    ) -> WorkspaceKind:
        if any(path in evidence for path in {"pnpm-workspace.yaml", "turbo.json", "nx.json", "go.work"}):
            return WorkspaceKind.MONOREPO
        if package_json.get("workspaces"):
            return WorkspaceKind.MONOREPO
        cargo_workspace = pyproject.get("tool", {}).get("uv", {}).get("workspace")
        if cargo_workspace:
            return WorkspaceKind.MONOREPO
        cargo_toml = self.workspace_root / "Cargo.toml"
        if cargo_toml.exists() and "[workspace]" in _read_text(cargo_toml):
            return WorkspaceKind.MONOREPO
        return WorkspaceKind.SINGLE_PROJECT if evidence else WorkspaceKind.UNKNOWN

    def _package_json(self) -> dict[str, Any]:
        path = self.workspace_root / "package.json"
        if not path.exists():
            return {}
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            return {}

    def _pyproject(self) -> dict[str, Any]:
        path = self.workspace_root / "pyproject.toml"
        if not path.exists():
            return {}
        try:
            return tomllib.loads(path.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError:
            return {}

    @staticmethod
    def _package_dependencies(package_json: dict[str, Any]) -> set[str]:
        dependencies: set[str] = set()
        for key in ("dependencies", "devDependencies", "peerDependencies"):
            payload = package_json.get(key) or {}
            if isinstance(payload, dict):
                dependencies.update(payload)
        return dependencies

    @staticmethod
    def _python_dependencies(pyproject: dict[str, Any]) -> set[str]:
        dependencies: set[str] = set()
        project = pyproject.get("project") or {}
        for value in project.get("dependencies") or []:
            dependencies.add(str(value).split("==")[0].split(">=")[0].lower())
        optional = project.get("optional-dependencies") or {}
        if isinstance(optional, dict):
            for values in optional.values():
                for value in values:
                    dependencies.add(str(value).split("==")[0].split(">=")[0].lower())
        dependency_groups = pyproject.get("dependency-groups") or {}
        if isinstance(dependency_groups, dict):
            for values in dependency_groups.values():
                for value in values:
                    dependencies.add(str(value).split("==")[0].split(">=")[0].lower())
        return dependencies

    def _test_frameworks(
        self,
        evidence: list[str],
        dependencies: set[str],
        commands: list[DiscoveredCommand],
    ) -> set[str]:
        frameworks = {
            command.name
            for command in commands
            if command.kind in {CheckKind.UNIT_TEST, CheckKind.INTEGRATION_TEST}
        }
        if any(path in evidence for path in {"pytest.ini", "tox.ini"}) or "pytest" in dependencies:
            frameworks.add("pytest")
        if (self.workspace_root / "tests").exists():
            frameworks.add("pytest")
        for candidate in ("jest", "vitest", "playwright", "cypress"):
            if candidate in dependencies:
                frameworks.add(candidate)
        return frameworks

    def _lint_tools(
        self,
        evidence: list[str],
        dependencies: set[str],
        commands: list[DiscoveredCommand],
    ) -> set[str]:
        tools = {command.name for command in commands if command.kind == CheckKind.LINT}
        if any(path in evidence for path in {"ruff.toml", ".ruff.toml"}) or "ruff" in dependencies:
            tools.add("ruff")
        if any("eslint" in path for path in evidence) or "eslint" in dependencies:
            tools.add("eslint")
        if "flake8" in dependencies:
            tools.add("flake8")
        return tools

    def _typecheck_tools(
        self,
        evidence: list[str],
        dependencies: set[str],
        commands: list[DiscoveredCommand],
    ) -> set[str]:
        tools = {command.name for command in commands if command.kind == CheckKind.TYPECHECK}
        if "tsconfig.json" in evidence:
            tools.add("tsc")
        for candidate in ("mypy", "pyright"):
            if candidate in dependencies or f"{candidate}.ini" in evidence:
                tools.add(candidate)
        return tools

    @staticmethod
    def _build_tools(
        evidence: list[str],
        commands: list[DiscoveredCommand],
    ) -> set[str]:
        tools = {command.name for command in commands if command.kind == CheckKind.BUILD}
        for file_name, tool in {
            "Cargo.toml": "cargo",
            "go.mod": "go",
            "pom.xml": "maven",
            "build.gradle": "gradle",
            "Makefile": "make",
            "pyproject.toml": "python-build",
        }.items():
            if file_name in evidence:
                tools.add(tool)
        return tools

    @staticmethod
    def _framework(dependencies: set[str]) -> str | None:
        for candidate in (
            "next",
            "vite",
            "react",
            "vue",
            "svelte",
            "django",
            "fastapi",
            "flask",
        ):
            if candidate in dependencies:
                return candidate
        return None


class CommandDiscovery:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)

    def discover(self) -> list[DiscoveredCommand]:
        commands: list[DiscoveredCommand] = []
        commands.extend(self._package_json_commands())
        commands.extend(self._python_commands())
        commands.extend(self._makefile_commands())
        commands.extend(self._justfile_commands())
        commands.extend(self._cargo_commands())
        commands.extend(self._go_commands())
        commands.extend(self._java_commands())
        return _dedupe_commands(commands)

    def _package_json_commands(self) -> list[DiscoveredCommand]:
        path = self.workspace_root / "package.json"
        if not path.exists():
            return []
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            return []
        scripts = payload.get("scripts") or {}
        if not isinstance(scripts, dict):
            return []
        package_manager = self._node_package_manager()
        commands = []
        for script, body in scripts.items():
            kind = _kind_from_name_and_body(script, str(body))
            if kind is None:
                continue
            purpose = _purpose_for_kind(kind)
            timeout = _timeout_for_kind(kind)
            commands.append(
                DiscoveredCommand(
                    name=script,
                    kind=kind,
                    request=CommandRequest(
                        argv=[package_manager, "run", script],
                        cwd=".",
                        purpose=purpose,
                        timeout_seconds=timeout,
                        filesystem_mode=FilesystemMode.READ_ONLY_WORKSPACE,
                    ),
                    source="package.json:scripts",
                    confidence=0.95,
                    description=str(body),
                )
            )
        return commands

    def _python_commands(self) -> list[DiscoveredCommand]:
        commands: list[DiscoveredCommand] = []
        pyproject = self.workspace_root / "pyproject.toml"
        pytest_config = any(
            (self.workspace_root / name).exists()
            for name in ("pytest.ini", "tox.ini")
        )
        pyproject_payload: dict[str, Any] = {}
        if pyproject.exists():
            try:
                pyproject_payload = tomllib.loads(pyproject.read_text(encoding="utf-8"))
            except tomllib.TOMLDecodeError:
                pyproject_payload = {}
        has_pytest = (
            pytest_config
            or bool(pyproject_payload.get("tool", {}).get("pytest"))
            or (self.workspace_root / "tests").exists()
            or _dependency_present(pyproject_payload, "pytest")
        )
        if has_pytest:
            test_target = "tests" if (self.workspace_root / "tests").exists() else "."
            commands.append(
                DiscoveredCommand(
                    name="pytest",
                    kind=CheckKind.UNIT_TEST,
                    request=CommandRequest(
                        argv=[
                            sys.executable,
                            "-m",
                            "pytest",
                            test_target,
                            "--basetemp",
                            "work/pytest-tmp",
                        ],
                        cwd=".",
                        purpose=CommandPurpose.PROJECT_VERIFICATION,
                        timeout_seconds=180,
                    ),
                    source="python:pytest",
                    confidence=0.9,
                    description="Run pytest for the project.",
                )
            )
        if self._has_ruff(pyproject_payload):
            commands.append(
                DiscoveredCommand(
                    name="ruff",
                    kind=CheckKind.LINT,
                    request=CommandRequest(
                        argv=[sys.executable, "-m", "ruff", "check", "."],
                        cwd=".",
                        purpose=CommandPurpose.LINT,
                        timeout_seconds=120,
                    ),
                    source="python:ruff",
                    confidence=0.75,
                    description="Run ruff check without modifying files.",
                )
            )
        if _dependency_present(pyproject_payload, "mypy") or (self.workspace_root / "mypy.ini").exists():
            commands.append(
                DiscoveredCommand(
                    name="mypy",
                    kind=CheckKind.TYPECHECK,
                    request=CommandRequest(
                        argv=[sys.executable, "-m", "mypy", "."],
                        cwd=".",
                        purpose=CommandPurpose.TYPECHECK,
                        timeout_seconds=180,
                    ),
                    source="python:mypy",
                    confidence=0.75,
                    description="Run mypy type checking.",
                )
            )
        if pyproject.exists():
            commands.append(
                DiscoveredCommand(
                    name="python-build",
                    kind=CheckKind.BUILD,
                    request=CommandRequest(
                        argv=[sys.executable, "-m", "build"],
                        cwd=".",
                        purpose=CommandPurpose.BUILD,
                        timeout_seconds=180,
                    ),
                    source="pyproject.toml",
                    confidence=0.55,
                    description="Build the Python package if build is installed.",
                )
            )
        return commands

    def _makefile_commands(self) -> list[DiscoveredCommand]:
        return self._target_file_commands("Makefile", "make")

    def _justfile_commands(self) -> list[DiscoveredCommand]:
        return self._target_file_commands("justfile", "just")

    def _target_file_commands(self, file_name: str, runner: str) -> list[DiscoveredCommand]:
        path = self.workspace_root / file_name
        if not path.exists():
            return []
        commands: list[DiscoveredCommand] = []
        for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
            match = re.match(r"^([A-Za-z0-9_.:-]+)\s*:", line)
            if not match:
                continue
            target = match.group(1)
            kind = _kind_from_name_and_body(target, "")
            if kind is None:
                continue
            commands.append(
                DiscoveredCommand(
                    name=target,
                    kind=kind,
                    request=CommandRequest(
                        argv=[runner, target],
                        cwd=".",
                        purpose=_purpose_for_kind(kind),
                        timeout_seconds=_timeout_for_kind(kind),
                    ),
                    source=file_name,
                    confidence=0.8,
                    description=f"{runner} target {target}",
                )
            )
        return commands

    def _cargo_commands(self) -> list[DiscoveredCommand]:
        if not (self.workspace_root / "Cargo.toml").exists():
            return []
        return [
            _simple_command("cargo-test", CheckKind.UNIT_TEST, ["cargo", "test"], "Cargo.toml"),
            _simple_command("cargo-build", CheckKind.BUILD, ["cargo", "build"], "Cargo.toml"),
        ]

    def _go_commands(self) -> list[DiscoveredCommand]:
        if not (self.workspace_root / "go.mod").exists():
            return []
        return [
            _simple_command("go-test", CheckKind.UNIT_TEST, ["go", "test", "./..."], "go.mod"),
            _simple_command("go-build", CheckKind.BUILD, ["go", "build", "./..."], "go.mod"),
        ]

    def _java_commands(self) -> list[DiscoveredCommand]:
        if (self.workspace_root / "pom.xml").exists():
            return [
                _simple_command("maven-test", CheckKind.UNIT_TEST, ["mvn", "test"], "pom.xml"),
                _simple_command("maven-package", CheckKind.BUILD, ["mvn", "package", "-DskipTests"], "pom.xml"),
            ]
        if (self.workspace_root / "build.gradle").exists():
            return [
                _simple_command("gradle-test", CheckKind.UNIT_TEST, ["gradle", "test"], "build.gradle"),
                _simple_command("gradle-build", CheckKind.BUILD, ["gradle", "build"], "build.gradle"),
            ]
        return []

    def _node_package_manager(self) -> str:
        for lockfile, manager in PACKAGE_LOCKS.items():
            if (self.workspace_root / lockfile).exists():
                return manager
        return "npm"

    def _has_ruff(self, pyproject: dict[str, Any]) -> bool:
        return (
            (self.workspace_root / "ruff.toml").exists()
            or (self.workspace_root / ".ruff.toml").exists()
            or bool(pyproject.get("tool", {}).get("ruff"))
            or _dependency_present(pyproject, "ruff")
        )


def _simple_command(
    name: str,
    kind: CheckKind,
    argv: list[str],
    source: str,
) -> DiscoveredCommand:
    return DiscoveredCommand(
        name=name,
        kind=kind,
        request=CommandRequest(
            argv=argv,
            cwd=".",
            purpose=_purpose_for_kind(kind),
            timeout_seconds=_timeout_for_kind(kind),
        ),
        source=source,
        confidence=0.85,
        description="Discovered from project configuration.",
    )


def _kind_from_name_and_body(name: str, body: str) -> CheckKind | None:
    lowered = f"{name} {body}".lower()
    name_lower = name.lower()
    if "build" in name_lower or "compile" in name_lower:
        return CheckKind.BUILD
    if any(token in lowered for token in ("e2e", "integration", "playwright", "cypress")):
        return CheckKind.INTEGRATION_TEST
    if "test" in lowered or "pytest" in lowered or "vitest" in lowered or "jest" in lowered:
        return CheckKind.UNIT_TEST
    if "typecheck" in lowered or "type-check" in lowered or "tsc" in lowered or "mypy" in lowered or "pyright" in lowered:
        return CheckKind.TYPECHECK
    if "lint" in lowered or "eslint" in lowered or "ruff check" in lowered:
        return CheckKind.LINT
    if "format" in lowered and ("check" in lowered or "--check" in lowered):
        return CheckKind.FORMAT
    if "build" in lowered or "compile" in lowered:
        return CheckKind.BUILD
    return None


def _purpose_for_kind(kind: CheckKind) -> CommandPurpose:
    if kind in {CheckKind.UNIT_TEST, CheckKind.INTEGRATION_TEST, CheckKind.SYNTAX}:
        return CommandPurpose.PROJECT_VERIFICATION
    if kind == CheckKind.BUILD:
        return CommandPurpose.BUILD
    if kind == CheckKind.LINT:
        return CommandPurpose.LINT
    if kind == CheckKind.TYPECHECK:
        return CommandPurpose.TYPECHECK
    if kind == CheckKind.FORMAT:
        return CommandPurpose.FORMAT_CHECK
    return CommandPurpose.PROJECT_VERIFICATION


def _timeout_for_kind(kind: CheckKind) -> float:
    return {
        CheckKind.INTEGRATION_TEST: 300.0,
        CheckKind.BUILD: 240.0,
        CheckKind.UNIT_TEST: 180.0,
        CheckKind.TYPECHECK: 180.0,
        CheckKind.LINT: 120.0,
        CheckKind.FORMAT: 120.0,
        CheckKind.SYNTAX: 60.0,
    }.get(kind, 120.0)


def _dependency_present(pyproject: dict[str, Any], name: str) -> bool:
    name = name.lower()
    detector = ProjectDetector._python_dependencies(pyproject)
    return name in detector


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return ""


def _dedupe_commands(commands: list[DiscoveredCommand]) -> list[DiscoveredCommand]:
    seen: set[tuple[CheckKind, str]] = set()
    deduped: list[DiscoveredCommand] = []
    for command in commands:
        key = (command.kind, command.request.display_command())
        if key in seen:
            continue
        seen.add(key)
        deduped.append(command)
    return deduped
