from __future__ import annotations

import os
from pathlib import Path

from singularity.command.models import (
    CommandDecision,
    CommandPolicyResult,
    CommandPurpose,
    CommandRequest,
    CommandRisk,
    FilesystemMode,
    NetworkMode,
)
READ_ONLY_GIT = {
    "branch",
    "diff",
    "log",
    "rev-parse",
    "show",
    "status",
}
MUTATING_GIT = {
    "add",
    "apply",
    "checkout",
    "clean",
    "commit",
    "merge",
    "pull",
    "push",
    "rebase",
    "reset",
    "restore",
    "switch",
}
PACKAGE_MANAGERS = {"cargo", "npm", "pip", "pnpm", "poetry", "uv", "yarn"}
NETWORK_TOOLS = {"curl", "scp", "sftp", "ssh", "wget"}
SYSTEM_MUTATORS = {
    "apt",
    "apt-get",
    "brew",
    "choco",
    "dnf",
    "pacman",
    "reg",
    "sc",
    "service",
    "sudo",
    "systemctl",
    "winget",
    "yum",
}
DESTRUCTIVE_PROGRAMS = {"del", "erase", "rmdir"}
FORMATTERS = {"black", "prettier", "ruff"}
LINTERS = {"eslint", "flake8", "ruff"}
TYPECHECKERS = {"mypy", "pyright", "tsc"}
BUILD_TOOLS = {"make", "tsc"}
TEST_TOOLS = {"pytest", "tox", "nox"}
READ_ONLY_PROGRAM_ALLOWLIST = {
    "git",
    "pwd",
    "whoami",
    "where",
    "which",
}
INTERPRETERS = {
    "bash",
    "cmd",
    "deno",
    "node",
    "perl",
    "powershell",
    "pwsh",
    "py",
    "python",
    "python3",
    "ruby",
    "sh",
    "zsh",
}


class CommandPolicy:
    def evaluate(
        self,
        request: CommandRequest,
        *,
        workspace_root: Path | str,
    ) -> CommandPolicyResult:
        risk_tags = self.classify(request)
        redaction_rules = [
            "*_TOKEN",
            "*_KEY",
            "*_SECRET",
            "PASSWORD",
            "DATABASE_URL",
            "DSN",
            "*_DSN",
            "CONN_STR",
            "*_CONN_STR",
            "CONN_STRING",
            "*_CONN_STRING",
            "CONNECTION_STRING",
            "*_CONNECTION_STRING",
            "AWS_*",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
        ]
        cwd_error = self._cwd_error(Path(workspace_root), request.cwd)
        if cwd_error is not None:
            return CommandPolicyResult(
                decision=CommandDecision.DENY,
                reasons=[cwd_error],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="cwd_outside_workspace",
            )

        if not request.argv and not request.shell:
            return CommandPolicyResult(
                decision=CommandDecision.DENY,
                reasons=["Command request must provide argv or shell."],
                risk_tags=sorted_risks({*risk_tags, CommandRisk.UNKNOWN}),
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="command_parse_error",
            )

        if CommandRisk.DESTRUCTIVE in risk_tags:
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Destructive commands require explicit review."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if CommandRisk.SYSTEM_MUTATION in risk_tags:
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["System mutation commands require explicit review."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if CommandRisk.NETWORK in risk_tags and request.network_mode == NetworkMode.DISABLED:
            return CommandPolicyResult(
                decision=CommandDecision.DENY,
                reasons=["Command has network risk but network mode is disabled."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="network_denied",
            )
        if request.shell is not None:
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Shell commands require review because parsing is delegated to the shell."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if CommandRisk.VCS_MUTATION in risk_tags:
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Git mutation commands must use GitClient or explicit review."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if CommandRisk.PACKAGE_MANAGER in risk_tags and not request.risk_acceptance_reason:
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Package manager commands can change dependency state and require review."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if (
            CommandRisk.WRITE_WORKSPACE in risk_tags
            and request.purpose != CommandPurpose.FORMATTER
            and not request.risk_acceptance_reason
        ):
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Workspace-writing commands require an explicit risk acceptance reason."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )
        if (
            CommandRisk.LONG_RUNNING in risk_tags
            and not request.risk_acceptance_reason
        ):
            return CommandPolicyResult(
                decision=CommandDecision.REQUIRE_REVIEW,
                reasons=["Long-running process sessions require explicit ownership."],
                risk_tags=risk_tags,
                required_network=request.network_mode,
                required_filesystem=request.filesystem_mode,
                redaction_rules=redaction_rules,
                error_code="review_required",
            )

        return CommandPolicyResult(
            decision=CommandDecision.ALLOW,
            reasons=["Command policy allowed execution."],
            risk_tags=risk_tags,
            required_network=request.network_mode,
            required_filesystem=request.filesystem_mode,
            redaction_rules=redaction_rules,
        )

    def classify(self, request: CommandRequest) -> list[CommandRisk]:
        tags: set[CommandRisk] = set()
        if request.purpose.name in CommandRisk.__members__:
            tags.add(CommandRisk[request.purpose.name])
        if request.shell is not None:
            tags.add(CommandRisk.UNKNOWN)
        if request.filesystem_mode in {
            FilesystemMode.READ_WRITE_WORKSPACE,
            FilesystemMode.READ_WRITE_SELECTED_PATHS,
            FilesystemMode.CACHE_MOUNT,
        }:
            tags.add(CommandRisk.WRITE_WORKSPACE)
        if request.network_mode != NetworkMode.DISABLED:
            tags.add(CommandRisk.NETWORK)
        if any(is_secret_env_name(name) for name in request.env_request):
            tags.add(CommandRisk.SECRET_RISK)
        argv = request.argv or []
        if not argv:
            return sorted_risks(tags or {CommandRisk.UNKNOWN})

        program = _program_name(argv)
        lowered = [part.lower() for part in argv]
        joined = " ".join(lowered)

        if (
            request.purpose
            in {
                CommandPurpose.PROJECT_VERIFICATION,
                CommandPurpose.LINT,
                CommandPurpose.TYPECHECK,
                CommandPurpose.FORMAT_CHECK,
            }
            or _is_test_command(program, lowered)
            or _is_lint_command(program, lowered)
            or _is_typecheck_command(program, lowered)
            or _is_format_check(program, lowered)
        ):
            tags.add(CommandRisk.PROJECT_VERIFICATION)
            tags.add(CommandRisk.EXECUTES_PROJECT_CODE)
        if request.purpose == CommandPurpose.BUILD or _is_build_command(program, lowered):
            tags.add(CommandRisk.BUILD)
            tags.add(CommandRisk.EXECUTES_PROJECT_CODE)
        if request.purpose == CommandPurpose.FORMATTER or _is_formatter(program, lowered):
            tags.add(CommandRisk.FORMATTER)
            tags.add(CommandRisk.WRITE_WORKSPACE)
        if request.purpose == CommandPurpose.LONG_RUNNING:
            tags.add(CommandRisk.LONG_RUNNING)
            tags.add(CommandRisk.EXECUTES_PROJECT_CODE)
        if _is_package_manager(program, lowered):
            tags.update(
                {
                    CommandRisk.PACKAGE_MANAGER,
                    CommandRisk.NETWORK,
                    CommandRisk.WRITE_WORKSPACE,
                }
            )
        if _is_network_command(program, lowered):
            tags.add(CommandRisk.NETWORK)
        if _is_git_read(program, lowered):
            tags.add(CommandRisk.VCS_READ)
        if _is_git_mutation(program, lowered):
            tags.add(CommandRisk.VCS_MUTATION)
        if request.purpose == CommandPurpose.DESTRUCTIVE or _is_destructive(program, lowered, joined):
            tags.add(CommandRisk.DESTRUCTIVE)
        if request.purpose == CommandPurpose.SYSTEM_MUTATION or program in SYSTEM_MUTATORS:
            tags.add(CommandRisk.SYSTEM_MUTATION)
        if not tags:
            tags.add(CommandRisk.UNKNOWN)
        return sorted_risks(tags)

    def requires_verification_runner(self, request: CommandRequest) -> bool:
        return _is_verification_like_request(request)

    @staticmethod
    def _cwd_error(workspace_root: Path, cwd: str) -> str | None:
        root = workspace_root.expanduser().resolve(strict=False)
        raw = Path(cwd)
        candidate = raw if raw.is_absolute() else root / raw
        try:
            resolved = candidate.resolve(strict=False)
            root_key = os.path.normcase(os.path.normpath(str(root)))
            candidate_key = os.path.normcase(os.path.normpath(str(resolved)))
            if os.path.commonpath([root_key, candidate_key]) != root_key:
                return f"cwd is outside workspace: {cwd}"
        except (OSError, ValueError) as exc:
            return f"cwd could not be resolved: {cwd}: {exc}"
        return None


def sorted_risks(tags: set[CommandRisk]) -> list[CommandRisk]:
    return sorted(tags, key=lambda tag: tag.value)


def is_secret_env_name(name: str) -> bool:
    upper = name.upper()
    return (
        upper.endswith("_TOKEN")
        or upper.endswith("_KEY")
        or upper.endswith("_SECRET")
        or upper.endswith("_DSN")
        or upper.endswith("_CONN_STR")
        or upper.endswith("_CONN_STRING")
        or upper.endswith("_CONNECTION_STRING")
        or "PASSWORD" in upper
        or upper == "DATABASE_URL"
        or upper in {"DSN", "CONN_STR", "CONN_STRING", "CONNECTION_STRING"}
        or upper.startswith("AWS_")
        or upper in {"GITHUB_TOKEN", "OPENAI_API_KEY"}
    )


def _is_verification_like_request(request: CommandRequest) -> bool:
    if request.purpose in {
        CommandPurpose.PROJECT_VERIFICATION,
        CommandPurpose.BUILD,
        CommandPurpose.LINT,
        CommandPurpose.TYPECHECK,
        CommandPurpose.FORMAT_CHECK,
    }:
        return True
    argv = request.argv or []
    if not argv:
        return False
    program = _program_name(argv)
    lowered = [part.lower() for part in argv]
    joined = " ".join(lowered)
    if program in {
        "pytest",
        "tox",
        "nox",
        "jest",
        "vitest",
        "tsc",
        "eslint",
        "mypy",
        "pyright",
    }:
        return True
    if program in {"python", "python3", "py"} and len(lowered) >= 3:
        return lowered[1:3] in (
            ["-m", "pytest"],
            ["-m", "mypy"],
            ["-m", "ruff"],
            ["-m", "build"],
        )
    if program in {"npm", "pnpm", "yarn"}:
        return any(part in {"test", "lint", "build", "typecheck", "type-check"} for part in lowered[1:3])
    if program == "cargo":
        return any(part in {"test", "build", "clippy", "check"} for part in lowered[1:3])
    if program == "go":
        return any(part in {"test", "build"} for part in lowered[1:3])
    if program in {"make", "just"}:
        return any(part in {"test", "lint", "build", "typecheck"} for part in lowered[1:3])
    return " pytest" in joined or " npm test" in joined or " pnpm test" in joined


def _program_name(argv: list[str]) -> str:
    program = Path(argv[0]).name.lower()
    for suffix in (".exe", ".cmd", ".bat", ".ps1"):
        if program.endswith(suffix):
            program = program[: -len(suffix)]
    return program


def _has_inline_execution(program: str, lowered: list[str]) -> bool:
    if program in {"python", "python3", "py", "node", "deno", "ruby", "perl"}:
        return any(part in {"-c", "-e"} for part in lowered[1:])
    if program in {"powershell", "pwsh"}:
        return any(part in {"-command", "-encodedcommand", "-enc"} for part in lowered[1:])
    if program == "cmd":
        return any(part in {"/c", "/k"} for part in lowered[1:])
    if program in {"bash", "sh", "zsh"}:
        return any(part in {"-c"} for part in lowered[1:])
    return False


def _is_test_command(program: str, lowered: list[str]) -> bool:
    if program in TEST_TOOLS:
        return True
    if program in {"python", "python3", "py"} and lowered[1:3] == ["-m", "pytest"]:
        return True
    return program in {"npm", "pnpm", "yarn"} and "test" in lowered[1:3]


def _is_build_command(program: str, lowered: list[str]) -> bool:
    if program in BUILD_TOOLS:
        return True
    if program in {"python", "python3", "py"} and lowered[1:3] == ["-m", "build"]:
        return True
    return program in {"npm", "pnpm", "yarn"} and "build" in lowered


def _is_lint_command(program: str, lowered: list[str]) -> bool:
    if program in LINTERS and "--fix" not in lowered:
        return True
    return program in {"npm", "pnpm", "yarn"} and "lint" in lowered[1:3]


def _is_typecheck_command(program: str, lowered: list[str]) -> bool:
    if program in TYPECHECKERS:
        return True
    return program in {"npm", "pnpm", "yarn"} and any(
        part in {"typecheck", "type-check", "tsc"} for part in lowered[1:3]
    )


def _is_format_check(program: str, lowered: list[str]) -> bool:
    if program in {"black", "prettier"} and any(part in {"--check", "-c", "check"} for part in lowered[1:]):
        return True
    if program == "ruff" and "format" in lowered and "--check" in lowered:
        return True
    return program in {"npm", "pnpm", "yarn"} and any(
        "format" in part and "check" in part for part in lowered[1:3]
    )


def _is_formatter(program: str, lowered: list[str]) -> bool:
    if _is_format_check(program, lowered):
        return False
    if program == "ruff" and "format" in lowered[1:]:
        return True
    if program in {"black", "prettier"}:
        return True
    return program == "eslint" and "--fix" in lowered


def _is_package_manager(program: str, lowered: list[str]) -> bool:
    if program in {"python", "python3", "py"} and lowered[1:3] == ["-m", "pip"]:
        return any(part in {"install", "uninstall"} for part in lowered[3:])
    if program == "uv" and any(part in {"add", "pip", "sync"} for part in lowered[1:]):
        return True
    if program not in PACKAGE_MANAGERS:
        return False
    return any(
        part in {"add", "install", "sync", "update", "upgrade"}
        for part in lowered[1:]
    )


def _is_network_command(program: str, lowered: list[str]) -> bool:
    if program in NETWORK_TOOLS:
        return True
    if program == "git":
        return any(part in {"clone", "fetch", "ls-remote", "pull", "push"} for part in lowered[1:3])
    return False


def _is_git_read(program: str, lowered: list[str]) -> bool:
    return program == "git" and len(lowered) > 1 and lowered[1] in READ_ONLY_GIT


def _is_git_mutation(program: str, lowered: list[str]) -> bool:
    return program == "git" and len(lowered) > 1 and lowered[1] in MUTATING_GIT


def _is_destructive(program: str, lowered: list[str], joined: str) -> bool:
    if program == "rm" and any(part in {"-rf", "-fr", "-r", "--recursive"} for part in lowered[1:]):
        return True
    if program in DESTRUCTIVE_PROGRAMS:
        return True
    if program == "remove-item" and any(part in {"-recurse", "-force"} for part in lowered[1:]):
        return True
    if program == "git" and len(lowered) > 1 and lowered[1] in {"clean", "reset"}:
        return True
    return "rm -rf" in joined or "remove-item" in joined and "-recurse" in joined
