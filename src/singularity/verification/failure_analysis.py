from __future__ import annotations

import shlex
from typing import Any
from uuid import uuid4

from singularity.failure_analysis.request import FailureAnalysisRequest
from singularity.failure_analysis.result import FailureAnalysisResult
from singularity.verification.contract import VerificationContract
from singularity.verification.models import CheckStatus, FailureType, VerificationResult


class RootCauseHypothesis:
    def __init__(self, description: str, evidence: list[str], confidence: float = 0.6) -> None:
        self.description = description
        self.evidence = evidence
        self.confidence = confidence


class NoProgressGuard:
    def __init__(self, *, max_same_failure_retries: int = 2) -> None:
        self.max_same_failure_retries = max_same_failure_retries
        self._seen: dict[str, int] = {}

    def record(self, fingerprint: str) -> str | None:
        count = self._seen.get(fingerprint, 0) + 1
        self._seen[fingerprint] = count
        if count > self.max_same_failure_retries:
            return "same_failure_retry_budget_exceeded"
        return None


class FailureAnalysisPipeline:
    def __init__(
        self,
        *,
        no_progress_guard: NoProgressGuard | None = None,
        max_same_failure_retries: int = 2,
    ) -> None:
        self.no_progress_guard = no_progress_guard or NoProgressGuard(
            max_same_failure_retries=max_same_failure_retries
        )

    def analyze_result(
        self,
        result: VerificationResult | dict[str, Any],
        *,
        changed_files: list[str],
        diff: str | None = None,
        task_contract: dict[str, Any] | None = None,
        next_verification_command: list[str] | None = None,
    ) -> FailureAnalysisResult:
        payload = result.to_dict() if hasattr(result, "to_dict") else dict(result)
        evidence = dict(payload.get("evidence") or {})
        parsed = [dict(item) for item in evidence.get("parsed_failures") or []]
        failure_type = _normalized_failure_type(
            str(payload.get("failure_type") or FailureType.UNKNOWN_FAILURE.value),
            parsed=parsed,
            command=evidence.get("command"),
        )
        suspect_files = _suspect_files(parsed=parsed, repair_hints=payload.get("repair_hints") or [], changed_files=changed_files)
        next_verification = {
            "check_id": payload.get("check_id"),
            "command": next_verification_command or _command_argv(evidence.get("command")),
        }
        root = _root_cause(
            failure_type=failure_type,
            parsed_failures=parsed,
            stdout=str(evidence.get("stdout_excerpt") or evidence.get("output_excerpt") or ""),
            stderr=str(evidence.get("stderr_excerpt") or ""),
            suspect_files=suspect_files,
        )
        fingerprint = _fingerprint(payload, parsed)
        no_progress = self.no_progress_guard.record(fingerprint)
        request = FailureAnalysisRequest(
            request_id=f"verification_failure_{uuid4().hex[:12]}",
            run_id="",
            session_id="",
            task_id="",
            phase_id="verification",
            workspace_root="",
            failure_source="verification",
            failure_summary=root.description,
            failure_sources=[],
            changed_files=list(changed_files),
            evidence_refs=[str(payload.get("check_id") or "verification")],
            metadata={
                "check_id": payload.get("check_id"),
                "next_verification": next_verification,
                "retrieval_queries": _retrieval_queries(
                    suspect_files=suspect_files,
                    root_cause=root,
                    task_contract=task_contract,
                    diff=diff,
                ),
            },
        )
        if no_progress is not None:
            return FailureAnalysisResult.blocked(
                request=request,
                reason=no_progress,
                category="verification_failed",
                affected_files=suspect_files,
            )
        command_value = next_verification.get("command")
        command = [str(item) for item in command_value] if isinstance(command_value, list) else []
        command_text = " ".join(str(item) for item in command)
        verification_plan = [command_text] if command_text else []
        return FailureAnalysisResult(
            analysis_id=f"failure_{uuid4().hex[:12]}",
            request_id=request.request_id,
            root_cause=root.description,
            failure_category=failure_type,
            affected_files=suspect_files,
            evidence_refs=request.evidence_refs,
            repair_strategy="repair_then_rerun",
            next_actions=[
                f"Inspect and patch {suspect_files[0] if suspect_files else 'the failing verification target'}."
            ],
            verification_plan=verification_plan,
            confidence=root.confidence,
            needs_user_input=False,
            verification_contract=VerificationContract.from_plan_strings(verification_plan),
        )

    def analyze_results(
        self,
        results: list[VerificationResult],
        *,
        changed_files: list[str],
        diff: str | None = None,
        task_contract: dict[str, Any] | None = None,
        verification_commands: dict[str, list[str]] | None = None,
    ) -> list[FailureAnalysisResult]:
        failed_statuses = {
            CheckStatus.FAILED.value,
            CheckStatus.BLOCKED.value,
            CheckStatus.TIMEOUT.value,
            CheckStatus.FLAKY.value,
        }
        return [
            self.analyze_result(
                result,
                changed_files=changed_files,
                diff=diff,
                task_contract=task_contract,
                next_verification_command=(verification_commands or {}).get(result.check_id),
            )
            for result in results
            if result.status.value in failed_statuses
        ]


def _root_cause(
    *,
    failure_type: str,
    parsed_failures: list[dict[str, Any]],
    stdout: str,
    stderr: str,
    suspect_files: list[str],
) -> RootCauseHypothesis:
    if parsed_failures:
        first = parsed_failures[0]
        location = first.get("file") or (suspect_files[0] if suspect_files else "verification output")
        message = str(first.get("message") or stdout or stderr or failure_type)
        return RootCauseHypothesis(
            description=f"{location}: {message}",
            evidence=[message],
            confidence=0.8 if first.get("file") else 0.55,
        )
    output = stdout or stderr or failure_type
    target = suspect_files[0] if suspect_files else "recent changes"
    return RootCauseHypothesis(
        description=f"{failure_type} in {target}: {output}",
        evidence=[output],
        confidence=0.45,
    )


def _suspect_files(
    *,
    parsed: list[dict[str, Any]],
    repair_hints: list[dict[str, Any]],
    changed_files: list[str],
) -> list[str]:
    files: list[str] = []
    for item in parsed:
        _append_unique(files, item.get("file"))
    for item in repair_hints:
        _append_unique(files, item.get("target_file"))
    if not files:
        for path in changed_files:
            _append_unique(files, path)
    return files[:5]


def _normalized_failure_type(
    failure_type: str,
    *,
    parsed: list[dict[str, Any]],
    command: Any,
) -> str:
    command_text = " ".join(_command_argv(command))
    if failure_type == FailureType.UNKNOWN_FAILURE.value and (
        "pytest" in command_text or any(item.get("test_name") for item in parsed)
    ):
        return FailureType.UNIT_TEST_FAILURE.value
    return failure_type


def _command_argv(command: Any) -> list[str]:
    if isinstance(command, list):
        return [str(item) for item in command]
    if isinstance(command, str) and command.strip():
        try:
            return shlex.split(command)
        except ValueError:
            return command.split()
    return []


def _fingerprint(payload: dict[str, Any], parsed: list[dict[str, Any]]) -> str:
    first = parsed[0] if parsed else {}
    return ":".join(
        [
            str(payload.get("check_id") or ""),
            str(payload.get("failure_type") or ""),
            str(first.get("file") or ""),
            str(first.get("line") or ""),
            str(first.get("test_name") or ""),
            str(first.get("message") or "")[:120],
        ]
    )


def _retrieval_queries(
    *,
    suspect_files: list[str],
    root_cause: RootCauseHypothesis,
    task_contract: dict[str, Any] | None,
    diff: str | None,
) -> list[str]:
    queries = [path for path in suspect_files]
    if task_contract:
        goal = task_contract.get("user_goal")
        if goal:
            queries.append(str(goal))
    if diff:
        queries.append(root_cause.description)
    return queries[:5]


def _append_unique(values: list[str], value: Any) -> None:
    if value and str(value) not in values:
        values.append(str(value))
