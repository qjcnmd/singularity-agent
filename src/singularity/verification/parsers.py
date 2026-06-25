from __future__ import annotations

import re
from abc import ABC, abstractmethod

from singularity.command import CommandResult, ExecutionStatus, SemanticStatus
from singularity.verification.models import (
    CheckKind,
    FailureType,
    ParsedFailure,
)


class FailureParser(ABC):
    @abstractmethod
    def parse(self, output: str) -> list[ParsedFailure]:
        raise NotImplementedError


class PytestFailureParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        failures: list[ParsedFailure] = []
        current_test: str | None = None
        for line in output.splitlines():
            failed_match = re.match(r"^FAILED\s+([^:\s]+)::([^\s]+)", line)
            if failed_match:
                current_test = failed_match.group(2)
                failures.append(
                    ParsedFailure(
                        file=failed_match.group(1),
                        line=None,
                        symbol=None,
                        test_name=current_test,
                        message=line.strip(),
                        stack_excerpt=None,
                    )
                )
                continue
            file_match = re.match(r"^([A-Za-z]:)?([^:\s]+\.py):(\d+):\s*(.+)$", line)
            if file_match:
                file_path = f"{file_match.group(1) or ''}{file_match.group(2)}"
                failures.append(
                    ParsedFailure(
                        file=file_path,
                        line=int(file_match.group(3)),
                        symbol=None,
                        test_name=current_test,
                        message=file_match.group(4).strip(),
                        stack_excerpt=line.strip(),
                    )
                )
        return _dedupe_failures(failures)


class PythonTracebackParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        failures: list[ParsedFailure] = []
        lines = output.splitlines()
        for index, line in enumerate(lines):
            match = re.search(r'File "([^"]+)", line (\d+), in ([^\s]+)', line)
            if not match:
                continue
            message = _first_error_after(lines, index) or line.strip()
            failures.append(
                ParsedFailure(
                    file=match.group(1),
                    line=int(match.group(2)),
                    symbol=match.group(3),
                    test_name=None,
                    message=message,
                    stack_excerpt="\n".join(lines[index : index + 4]),
                )
            )
        return _dedupe_failures(failures)


class TscFailureParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        failures = []
        pattern = re.compile(r"^(.+?)\((\d+),(\d+)\):\s+error\s+(TS\d+):\s+(.+)$")
        for line in output.splitlines():
            match = pattern.match(line.strip())
            if not match:
                continue
            failures.append(
                ParsedFailure(
                    file=match.group(1),
                    line=int(match.group(2)),
                    symbol=match.group(4),
                    test_name=None,
                    message=match.group(5),
                    stack_excerpt=line.strip(),
                )
            )
        return failures


class EslintFailureParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        failures: list[ParsedFailure] = []
        current_file: str | None = None
        for line in output.splitlines():
            stripped = line.strip()
            if stripped and not stripped.startswith(("error", "warning")) and re.search(r"\.(js|jsx|ts|tsx)$", stripped):
                current_file = stripped
                continue
            match = re.match(r"^(\d+):(\d+)\s+(error|warning)\s+(.+?)\s+([A-Za-z0-9@/_-]+)$", stripped)
            if match:
                failures.append(
                    ParsedFailure(
                        file=current_file,
                        line=int(match.group(1)),
                        symbol=match.group(5),
                        test_name=None,
                        message=match.group(4),
                        stack_excerpt=line.strip(),
                    )
                )
        return failures


class NpmBuildFailureParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        failures: list[ParsedFailure] = []
        for line in output.splitlines():
            if "error" not in line.lower() and "failed" not in line.lower():
                continue
            match = re.search(r"([^\s:]+\.(tsx?|jsx?|css|json)):(\d+):(\d+)", line)
            failures.append(
                ParsedFailure(
                    file=match.group(1) if match else None,
                    line=int(match.group(3)) if match else None,
                    symbol=None,
                    test_name=None,
                    message=line.strip(),
                    stack_excerpt=line.strip(),
                )
            )
            if len(failures) >= 5:
                break
        return failures


class GenericFailureParser(FailureParser):
    def parse(self, output: str) -> list[ParsedFailure]:
        for line in output.splitlines():
            stripped = line.strip()
            if stripped:
                return [
                    ParsedFailure(
                        file=None,
                        line=None,
                        symbol=None,
                        test_name=None,
                        message=stripped,
                        stack_excerpt=stripped,
                    )
                ]
        return []


class FailureParserRegistry:
    def __init__(self) -> None:
        self.parsers: list[FailureParser] = [
            PytestFailureParser(),
            PythonTracebackParser(),
            TscFailureParser(),
            EslintFailureParser(),
            NpmBuildFailureParser(),
        ]
        self.generic = GenericFailureParser()

    def parse(self, output: str) -> list[ParsedFailure]:
        failures: list[ParsedFailure] = []
        for parser in self.parsers:
            failures.extend(parser.parse(output))
        return _dedupe_failures(failures) or self.generic.parse(output)


def classify_failure(
    *,
    check_kind: CheckKind,
    command_result: CommandResult | None,
    parsed_failures: list[ParsedFailure],
) -> FailureType | None:
    if command_result is None:
        return FailureType.CHECK_BLOCKED
    if command_result.error_code == "sandbox_unavailable":
        return FailureType.SANDBOX_LIMITATION
    if command_result.error_code == "sandbox_violation":
        return FailureType.SANDBOX_VIOLATION
    if command_result.timed_out or command_result.execution_status == ExecutionStatus.TIMED_OUT:
        return FailureType.TIMEOUT
    if command_result.error_code == "command_not_found":
        return FailureType.MISSING_COMMAND
    if command_result.error_code == "permission_error":
        return FailureType.PERMISSION_DENIED
    if command_result.execution_status in {
        ExecutionStatus.POLICY_DENIED,
        ExecutionStatus.REVIEW_REQUIRED,
    }:
        return FailureType.CHECK_REVIEW_REQUIRED
    if command_result.execution_status == ExecutionStatus.SPAWN_FAILED:
        return FailureType.MISSING_COMMAND
    if command_result.execution_status != ExecutionStatus.COMPLETED:
        return FailureType.ENVIRONMENT_ERROR
    if command_result.semantic_status == SemanticStatus.SUCCEEDED:
        return None
    if any(_looks_like_syntax(failure.message) for failure in parsed_failures):
        return FailureType.SYNTAX_ERROR
    if check_kind == CheckKind.TYPECHECK:
        return FailureType.TYPE_ERROR
    if check_kind == CheckKind.LINT:
        return FailureType.LINT_ERROR
    if check_kind == CheckKind.FORMAT:
        return FailureType.FORMAT_ERROR
    if check_kind == CheckKind.UNIT_TEST:
        return FailureType.UNIT_TEST_FAILURE
    if check_kind == CheckKind.INTEGRATION_TEST:
        return FailureType.INTEGRATION_TEST_FAILURE
    if check_kind == CheckKind.BUILD:
        return FailureType.BUILD_FAILURE
    if command_result.semantic_status == SemanticStatus.BUILD_FAILED:
        return FailureType.BUILD_FAILURE
    if command_result.semantic_status == SemanticStatus.LINT_FAILED:
        return FailureType.LINT_ERROR
    if command_result.semantic_status == SemanticStatus.TYPECHECK_FAILED:
        return FailureType.TYPE_ERROR
    return FailureType.UNKNOWN_FAILURE


def _first_error_after(lines: list[str], index: int) -> str | None:
    for line in lines[index + 1 :]:
        stripped = line.strip()
        if re.match(r"^[A-Za-z_]*Error:", stripped) or stripped.startswith("E   "):
            return stripped
    return None


def _looks_like_syntax(message: str) -> bool:
    lowered = message.lower()
    return "syntaxerror" in lowered or "parse error" in lowered or "unexpected token" in lowered


def _dedupe_failures(failures: list[ParsedFailure]) -> list[ParsedFailure]:
    seen: set[tuple[str | None, int | None, str | None, str]] = set()
    deduped = []
    for failure in failures:
        key = (failure.file, failure.line, failure.test_name, failure.message)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(failure)
    return deduped[:20]
