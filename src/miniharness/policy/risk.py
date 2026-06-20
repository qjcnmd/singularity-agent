from __future__ import annotations

import os
import re
import shlex
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlparse

from miniharness.policy.models import (
    Capability,
    OperationKind,
    PolicyRequest,
    ResourceRef,
    RiskLevel,
    RiskTag,
)


SECRET_NAME_RE = re.compile(
    r"(^\.env(\.|$)|id_rsa|credentials|credential|token|secret|api[_-]?key|password|\.pem$|\.pfx$|\.p12$|\.key$)",
    re.IGNORECASE,
)
LOCKFILE_NAMES = {
    "cargo.lock",
    "go.sum",
    "package-lock.json",
    "pnpm-lock.yaml",
    "poetry.lock",
    "requirements.txt",
    "uv.lock",
    "yarn.lock",
}
CONFIG_NAMES = {
    ".pre-commit-config.yaml",
    "dockerfile",
    "makefile",
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "tox.ini",
    "tsconfig.json",
}
PACKAGE_PROGRAMS = {"cargo", "npm", "pip", "pnpm", "poetry", "uv", "yarn"}
NETWORK_PROGRAMS = {"curl", "wget", "invoke-webrequest", "iwr"}
LONG_RUNNING_MARKERS = {
    "npm run dev",
    "pnpm dev",
    "yarn dev",
    "vite",
    "python -m http.server",
    "uvicorn",
}
SHELL_EXPANSION_PATTERNS = ("|", ">", "<", "&&", "||", ";", "$(", "`")
INLINE_WRITE_MARKERS = (
    ".write_text(",
    ".write_bytes(",
    ".open(",
    "open(",
    "touch(",
    "unlink(",
    "rename(",
    "replace(",
    "remove(",
    "rmdir(",
    "mkdir(",
    "makedirs(",
    "shutil.copy",
    "shutil.move",
    "writefilesync(",
    "appendfilesync(",
    "rm(",
    "mkdirsync(",
)
INLINE_WRITE_MODE_MARKERS = (
    '"w"',
    "'w'",
    '"a"',
    "'a'",
    '"x"',
    "'x'",
    '"w+"',
    "'w+'",
    '"a+"',
    "'a+'",
)


@dataclass(frozen=True)
class RiskAssessment:
    level: RiskLevel
    tags: list[RiskTag]
    reasons: list[str]


class RiskClassifier:
    def __init__(self, workspace_root: Path | str) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)

    def classify(self, request: PolicyRequest) -> RiskAssessment:
        tags = set(_coerce_tag(tag) for tag in request.risk_tags if _is_known_tag(tag))
        reasons: list[str] = []
        level = RiskLevel.NONE

        if request.resource.resource_type in {"file", "directory", "workspace", "config"}:
            path_level, path_tags, path_reasons = self._classify_path(
                request.resource,
                operation=request.operation,
            )
            level = max_level(level, path_level)
            tags.update(path_tags)
            reasons.extend(path_reasons)

        if request.resource.resource_type == "command" or request.operation in {
            OperationKind.EXECUTE_COMMAND,
            OperationKind.EXECUTE_PROJECT_CODE,
            OperationKind.PACKAGE_INSTALL,
            OperationKind.START_LONG_PROCESS,
            OperationKind.VERIFICATION,
        }:
            command = str(
                request.metadata.get("command")
                or request.metadata.get("shell")
                or request.resource.identifier
            )
            argv = _metadata_argv(request.metadata.get("argv"))
            command_level, command_tags, command_reasons = self._classify_command(
                command,
                operation=request.operation,
                capability=request.capability,
                argv=argv,
            )
            level = max_level(level, command_level)
            tags.update(command_tags)
            reasons.extend(command_reasons)

        if request.requires_network:
            tags.add(RiskTag.NETWORK)
            level = max_level(level, RiskLevel.MEDIUM)
        if request.touches_secrets:
            tags.add(RiskTag.SECRET_ACCESS)
            level = max_level(level, RiskLevel.HIGH)
        if request.destructive:
            tags.add(RiskTag.DESTRUCTIVE)
            level = max_level(level, RiskLevel.HIGH)
        if request.long_running:
            tags.add(RiskTag.LONG_RUNNING)
            level = max_level(level, RiskLevel.MEDIUM)
        if request.capability == Capability.EXECUTE_GENERATED_CODE:
            tags.add(RiskTag.EXECUTES_GENERATED_CODE)
            level = max_level(level, RiskLevel.HIGH)

        if not tags and request.operation in {
            OperationKind.READ_FILE,
            OperationKind.LIST_DIRECTORY,
            OperationKind.SEARCH,
        }:
            tags.add(RiskTag.WORKSPACE_READ)
            level = max_level(level, RiskLevel.LOW)

        return RiskAssessment(
            level=level if level != RiskLevel.NONE or not tags else RiskLevel.LOW,
            tags=sorted(tags, key=lambda tag: tag.value),
            reasons=reasons or ["No elevated risk detected."],
        )

    def _classify_path(
        self, resource: ResourceRef, *, operation: OperationKind
    ) -> tuple[RiskLevel, set[RiskTag], list[str]]:
        tags: set[RiskTag] = set()
        reasons: list[str] = []
        raw = Path(resource.identifier).expanduser()
        candidate = raw if raw.is_absolute() else self.workspace_root / raw
        resolved = candidate.resolve(strict=False)
        inside = is_inside(resolved, self.workspace_root)
        name = resolved.name.lower()
        parts = {part.lower() for part in resolved.parts}
        relative_text = str(resolved.relative_to(self.workspace_root)) if inside else str(resolved)
        level = RiskLevel.LOW if inside else RiskLevel.HIGH

        if inside:
            tags.add(RiskTag.WORKSPACE_READ)
        else:
            tags.add(RiskTag.OUTSIDE_WORKSPACE)
            reasons.append("Path is outside workspace.")

        if name == ".env" or name.startswith(".env.") or SECRET_NAME_RE.search(name) or SECRET_NAME_RE.search(relative_text):
            tags.add(RiskTag.SECRET_ACCESS)
            level = max_level(level, RiskLevel.HIGH)
            reasons.append("Path looks sensitive.")
        if name == "id_rsa" or ".ssh" in parts or _is_system_or_browser_path(resolved):
            level = RiskLevel.CRITICAL
            tags.add(RiskTag.SECRET_ACCESS)
            reasons.append("Path targets key, browser, or system data.")
        if operation in {
            OperationKind.MUTATE_FILE,
            OperationKind.CREATE_FILE,
            OperationKind.CHANGE_CONFIG,
        }:
            tags.add(RiskTag.MUTATES_FILES)
            if name in CONFIG_NAMES or ".github" in parts:
                tags.add(RiskTag.MUTATES_CONFIG)
                level = max_level(level, RiskLevel.MEDIUM)
            if name in LOCKFILE_NAMES:
                tags.add(RiskTag.MUTATES_LOCKFILE)
                level = max_level(level, RiskLevel.HIGH)
        if operation == OperationKind.DELETE_FILE:
            tags.update({RiskTag.MUTATES_FILES, RiskTag.DESTRUCTIVE, RiskTag.IRREVERSIBLE})
            level = RiskLevel.CRITICAL if not inside else RiskLevel.HIGH
            reasons.append("Delete operations require elevated review.")
        return level, tags, reasons

    def _classify_command(
        self,
        command: str,
        *,
        operation: OperationKind,
        capability: Capability,
        argv: list[str] | None = None,
    ) -> tuple[RiskLevel, set[RiskTag], list[str]]:
        tags: set[RiskTag] = set()
        reasons: list[str] = []
        lowered = command.lower()
        argv = argv or _split_command(command)
        program = Path(argv[0]).name.lower() if argv else ""
        for suffix in (".exe", ".cmd", ".bat", ".ps1"):
            if program.endswith(suffix):
                program = program[: -len(suffix)]
        level = RiskLevel.LOW

        if operation in {OperationKind.VERIFICATION, OperationKind.EXECUTE_PROJECT_CODE} or _is_verification(program, lowered, argv):
            tags.update({RiskTag.EXECUTES_CODE, RiskTag.EXECUTES_PROJECT_CODE})
            level = max_level(level, RiskLevel.MEDIUM)
        if capability == Capability.EXECUTE_GENERATED_CODE:
            tags.update({RiskTag.EXECUTES_CODE, RiskTag.EXECUTES_GENERATED_CODE})
            level = max_level(level, RiskLevel.HIGH)
        if any(pattern in lowered for pattern in SHELL_EXPANSION_PATTERNS):
            tags.add(RiskTag.SHELL_EXPANSION)
            level = max_level(level, RiskLevel.MEDIUM)
            reasons.append("Command uses shell expansion.")
        if _has_inline_workspace_write(program, lowered, argv):
            tags.add(RiskTag.MUTATES_FILES)
            level = max_level(level, RiskLevel.MEDIUM)
            reasons.append("Inline interpreter code appears to write files.")
        if _is_destructive_command(program, lowered, argv):
            tags.update({RiskTag.DESTRUCTIVE, RiskTag.IRREVERSIBLE, RiskTag.MUTATES_FILES})
            level = RiskLevel.CRITICAL
            reasons.append("Command is destructive.")
        if program in {"sudo", "runas"} or " -encodedcommand" in lowered or " -enc " in lowered:
            level = RiskLevel.CRITICAL
            reasons.append("Command requests elevated or encoded execution.")
        if _is_package_manager(program, argv):
            tags.update(
                {
                    RiskTag.PACKAGE_MANAGER,
                    RiskTag.SUPPLY_CHAIN,
                    RiskTag.NETWORK,
                    RiskTag.MUTATES_LOCKFILE,
                    RiskTag.MUTATES_FILES,
                }
            )
            level = max_level(level, RiskLevel.HIGH)
            reasons.append("Package manager command changes dependency state.")
        if program in NETWORK_PROGRAMS:
            tags.add(RiskTag.NETWORK)
            level = max_level(level, RiskLevel.MEDIUM)
        if re.search(r"\b(curl|wget|invoke-webrequest|iwr)\b.*\|\s*(sh|bash|powershell|pwsh)", lowered):
            tags.update({RiskTag.NETWORK, RiskTag.SUPPLY_CHAIN, RiskTag.EXECUTES_CODE})
            level = RiskLevel.CRITICAL
            reasons.append("Remote script is piped into an interpreter.")
        if any(marker in lowered for marker in LONG_RUNNING_MARKERS):
            tags.update({RiskTag.LONG_RUNNING, RiskTag.PERSISTENT_SIDE_EFFECT})
            level = max_level(level, RiskLevel.MEDIUM)
        if operation == OperationKind.NETWORK_ACCESS:
            tags.add(RiskTag.NETWORK)
            level = max_level(level, RiskLevel.MEDIUM)
        if operation == OperationKind.START_LONG_PROCESS:
            tags.add(RiskTag.LONG_RUNNING)
            level = max_level(level, RiskLevel.MEDIUM)
        return level, tags, reasons


def is_inside(path: Path, root: Path) -> bool:
    try:
        root_key = os.path.normcase(os.path.normpath(str(root.resolve(strict=False))))
        path_key = os.path.normcase(os.path.normpath(str(path.resolve(strict=False))))
        return os.path.commonpath([root_key, path_key]) == root_key
    except (OSError, ValueError):
        return False


def max_level(left: RiskLevel, right: RiskLevel) -> RiskLevel:
    order = [
        RiskLevel.NONE,
        RiskLevel.LOW,
        RiskLevel.MEDIUM,
        RiskLevel.HIGH,
        RiskLevel.CRITICAL,
    ]
    return order[max(order.index(left), order.index(right))]


def _split_command(command: str) -> list[str]:
    try:
        return shlex.split(command, posix=os.name != "nt")
    except ValueError:
        return command.split()


def _metadata_argv(value: object) -> list[str] | None:
    if not isinstance(value, list):
        return None
    return [str(item) for item in value]


def _is_verification(program: str, lowered: str, argv: list[str]) -> bool:
    if program in {"pytest", "tox", "nox", "jest", "vitest", "tsc", "eslint", "mypy", "pyright"}:
        return True
    if program in {"python", "python3", "py"} and len(argv) >= 3:
        return argv[1:3] in (["-m", "pytest"], ["-m", "mypy"], ["-m", "ruff"], ["-m", "build"])
    if program in {"npm", "pnpm", "yarn"}:
        return any(part in {"test", "lint", "build", "typecheck", "type-check"} for part in argv[1:3])
    return " pytest" in lowered or " npm test" in lowered or " pnpm test" in lowered


def _is_package_manager(program: str, argv: list[str]) -> bool:
    lowered = [part.lower() for part in argv]
    if program in {"python", "python3", "py"} and lowered[1:3] == ["-m", "pip"]:
        return any(part in {"install", "uninstall"} for part in lowered[3:])
    if program == "uv" and any(part in {"add", "pip", "sync"} for part in lowered[1:]):
        return True
    if program not in PACKAGE_PROGRAMS:
        return False
    return any(part in {"add", "install", "sync", "update", "upgrade"} for part in lowered[1:])


def _has_inline_workspace_write(program: str, lowered: str, argv: list[str]) -> bool:
    inline_code = _inline_code(program, argv)
    if inline_code is None:
        return False
    code = inline_code.lower()
    if any(marker in code for marker in INLINE_WRITE_MARKERS):
        return True
    return "open(" in code and any(marker in code for marker in INLINE_WRITE_MODE_MARKERS)


def _inline_code(program: str, argv: list[str]) -> str | None:
    if program in {"python", "python3", "py", "node", "deno", "ruby", "perl"}:
        flags = {"-c", "-e"}
    elif program in {"bash", "sh", "zsh"}:
        flags = {"-c"}
    elif program in {"powershell", "pwsh"}:
        flags = {"-command", "-encodedcommand", "-enc"}
    elif program == "cmd":
        flags = {"/c", "/k"}
    else:
        return None
    lowered = [part.lower() for part in argv]
    for index, part in enumerate(lowered[1:], start=1):
        if part in flags and index + 1 < len(argv):
            return argv[index + 1]
    return None


def _is_destructive_command(program: str, lowered: str, argv: list[str]) -> bool:
    lowered_args = [part.lower() for part in argv]
    if program == "rm" and any(part in {"-rf", "-fr", "-r", "--recursive"} for part in lowered_args[1:]):
        return True
    if program in {"del", "erase", "rmdir"}:
        return True
    if program == "remove-item" and any(part in {"-recurse", "-force"} for part in lowered_args[1:]):
        return True
    return "rm -rf" in lowered or ("remove-item" in lowered and "-recurse" in lowered)


def _is_system_or_browser_path(path: Path) -> bool:
    lowered = str(path).lower()
    markers = [
        "/etc/",
        "/system/",
        "/windows/system32",
        "appdata/local/google/chrome/user data",
        "appdata/roaming/mozilla/firefox",
        "library/application support/google/chrome",
    ]
    return any(marker in lowered.replace("\\", "/") for marker in markers)


def _is_known_tag(tag: object) -> bool:
    return isinstance(tag, RiskTag) or str(tag) in RiskTag.__members__ or str(tag) in RiskTag._value2member_map_


def _coerce_tag(tag: object) -> RiskTag:
    if isinstance(tag, RiskTag):
        return tag
    text = str(tag)
    if text in RiskTag.__members__:
        return RiskTag[text]
    return RiskTag(text)


def host_from_url(value: str) -> str:
    parsed = urlparse(value)
    return parsed.netloc or value
