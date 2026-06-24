from __future__ import annotations

import shlex
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.verification.models import CheckStatus, FailureType, VerificationResult


@dataclass(frozen=True)
class RootCauseHypothesis:
    description: str
    evidence: list[str]
    confidence: float = 0.6

    def to_dict(self) -> dict[str, Any]:
        return {
            "description": self.description,
            "evidence": self.evidence,
            "confidence": self.confidence,
        }


@dataclass(frozen=True)
class RepairStep:
    step_id: str
    action: str
    target_file: str | None
    rationale: str
    next_verification: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "step_id": self.step_id,
            "action": self.action,
            "target_file": self.target_file,
            "rationale": self.rationale,
            "next_verification": self.next_verification,
        }


@dataclass(frozen=True)
class RepairPlan:
    plan_id: str
    strategy: str
    summary: str
    steps: list[RepairStep]
    next_verification: dict[str, Any]
    blocked_reason: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "strategy": self.strategy,
            "summary": self.summary,
            "steps": [step.to_dict() for step in self.steps],
            "next_verification": self.next_verification,
            "blocked_reason": self.blocked_reason,
        }


@dataclass(frozen=True)
class FailureAnalysis:
    analysis_id: str
    check_id: str
    failure_type: str
    root_cause: RootCauseHypothesis
    hypotheses: list[RootCauseHypothesis]
    suspect_files: list[str]
    repair_plan: RepairPlan
    next_verification: dict[str, Any]
    no_progress_reason: str | None = None
    retrieval_queries: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "analysis_id": self.analysis_id,
            "check_id": self.check_id,
            "failure_type": self.failure_type,
            "root_cause": self.root_cause.to_dict(),
            "hypotheses": [hypothesis.to_dict() for hypothesis in self.hypotheses],
            "suspect_files": self.suspect_files,
            "repair_plan": self.repair_plan.to_dict(),
            "next_verification": self.next_verification,
            "no_progress_reason": self.no_progress_reason,
            "retrieval_queries": self.retrieval_queries,
        }


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


class RepairPlanner:
    def plan(self, analysis: FailureAnalysis | list[FailureAnalysis]) -> RepairPlan:
        analyses = analysis if isinstance(analysis, list) else [analysis]
        if not analyses:
            return _empty_plan()
        first = analyses[0]
        blocked = next((item for item in analyses if item.no_progress_reason), None)
        if blocked is not None:
            return RepairPlan(
                plan_id=f"repair_{uuid4().hex[:12]}",
                strategy="stop_and_ask",
                summary=f"No progress guard blocked repair: {blocked.no_progress_reason}.",
                steps=[],
                next_verification=blocked.next_verification,
                blocked_reason=blocked.no_progress_reason,
            )
        steps: list[RepairStep] = []
        for index, item in enumerate(analyses, start=1):
            target = item.suspect_files[0] if item.suspect_files else None
            steps.append(
                RepairStep(
                    step_id=f"repair_step_{index}",
                    action="inspect_and_patch",
                    target_file=target,
                    rationale=item.root_cause.description,
                    next_verification=item.next_verification,
                )
            )
        return RepairPlan(
            plan_id=f"repair_{uuid4().hex[:12]}",
            strategy="repair_then_rerun",
            summary="Repair suspected files, then rerun the bound verification command.",
            steps=steps,
            next_verification=first.next_verification,
        )


class FailureAnalysisPipeline:
    def __init__(
        self,
        *,
        no_progress_guard: NoProgressGuard | None = None,
        max_same_failure_retries: int = 2,
        repair_planner: RepairPlanner | None = None,
    ) -> None:
        self.no_progress_guard = no_progress_guard or NoProgressGuard(
            max_same_failure_retries=max_same_failure_retries
        )
        self.repair_planner = repair_planner or RepairPlanner()

    def analyze_result(
        self,
        result: VerificationResult | dict[str, Any],
        *,
        changed_files: list[str],
        diff: str | None = None,
        task_contract: dict[str, Any] | None = None,
        next_verification_command: list[str] | None = None,
    ) -> FailureAnalysis:
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
        analysis = FailureAnalysis(
            analysis_id=f"failure_{uuid4().hex[:12]}",
            check_id=str(payload.get("check_id") or ""),
            failure_type=failure_type,
            root_cause=root,
            hypotheses=[root],
            suspect_files=suspect_files,
            repair_plan=_empty_plan(next_verification=next_verification),
            next_verification=next_verification,
            no_progress_reason=no_progress,
            retrieval_queries=_retrieval_queries(suspect_files=suspect_files, root_cause=root, task_contract=task_contract, diff=diff),
        )
        return FailureAnalysis(
            analysis_id=analysis.analysis_id,
            check_id=analysis.check_id,
            failure_type=analysis.failure_type,
            root_cause=analysis.root_cause,
            hypotheses=analysis.hypotheses,
            suspect_files=analysis.suspect_files,
            repair_plan=self.repair_planner.plan(analysis),
            next_verification=analysis.next_verification,
            no_progress_reason=analysis.no_progress_reason,
            retrieval_queries=analysis.retrieval_queries,
        )

    def analyze_results(
        self,
        results: list[VerificationResult],
        *,
        changed_files: list[str],
        diff: str | None = None,
        task_contract: dict[str, Any] | None = None,
        verification_commands: dict[str, list[str]] | None = None,
    ) -> list[FailureAnalysis]:
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


def _empty_plan(next_verification: dict[str, Any] | None = None) -> RepairPlan:
    return RepairPlan(
        plan_id=f"repair_{uuid4().hex[:12]}",
        strategy="none",
        summary="No repair plan is available.",
        steps=[],
        next_verification=next_verification or {"check_id": None, "command": []},
    )
