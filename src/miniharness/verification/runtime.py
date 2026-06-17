from __future__ import annotations

import sys
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from miniharness.command import (
    CommandPurpose,
    CommandRequest,
    CommandRuntime,
    ExecutionStatus,
    FilesystemMode,
    ResourceLimits,
    SemanticStatus,
)
from miniharness.trace import TraceWriter
from miniharness.verification.assessor import CompletionAssessor
from miniharness.verification.discovery import ProjectDetector
from miniharness.verification.impact import ImpactAnalyzer
from miniharness.verification.models import (
    CheckKind,
    CheckStatus,
    CompletionAssessment,
    FailureType,
    ImpactAnalysis,
    ProjectProfile,
    VerificationCheck,
    VerificationDecision,
    VerificationEvidence,
    VerificationPlan,
    VerificationResult,
)
from miniharness.verification.parsers import FailureParserRegistry, classify_failure
from miniharness.verification.policy import VerificationPolicy
from miniharness.verification.repair import RepairHintGenerator, RepairLoopController


class VerificationRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        command_runtime: CommandRuntime | None = None,
        trace: TraceWriter | None = None,
        policy: VerificationPolicy | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.command_runtime = command_runtime or CommandRuntime(self.workspace_root, trace=trace)
        self.trace = trace
        self.policy = policy or VerificationPolicy(self.command_runtime.policy)
        self.parsers = FailureParserRegistry()
        self.hints = RepairHintGenerator()
        self.assessor = CompletionAssessor()
        self.repair_loop = RepairLoopController()
        self._plans: dict[str, VerificationPlan] = {}
        self._results: dict[str, list[VerificationResult]] = {}
        self._assessments: dict[str, CompletionAssessment] = {}
        self._latest_plan_id: str | None = None

    def plan_verification(
        self,
        *,
        changed_files: list[str],
        task_intent: str,
        transaction_id: str | None = None,
        changeset_id: str | None = None,
    ) -> VerificationPlan:
        profile = ProjectDetector(self.workspace_root).detect()
        impact = ImpactAnalyzer().analyze(
            changed_files=changed_files,
            task_intent=task_intent,
            project_profile=profile,
            transaction_id=transaction_id,
            changeset_id=changeset_id,
        )
        plan = self._build_plan(
            profile=profile,
            impact=impact,
            transaction_id=transaction_id,
            changeset_id=changeset_id,
        )
        self._plans[plan.id] = plan
        self._results.setdefault(plan.id, [])
        self._latest_plan_id = plan.id
        self._record_trace(
            "plan",
            {
                "verification_plan_id": plan.id,
                "transaction_id": transaction_id,
                "changeset_id": changeset_id,
                "project_profile": profile.to_dict(),
                "impact_analysis": impact.to_dict(),
                "plan": plan.to_dict(),
            },
        )
        return plan

    def run_plan(
        self,
        plan_id: str | None = None,
    ) -> dict[str, Any]:
        plan = self._plan(plan_id)
        started = time.perf_counter()
        results: list[VerificationResult] = []
        for check in plan.skipped_checks:
            results.append(self._skipped_result(check))
        for check in plan.blocked_checks:
            results.append(self._blocked_result(check))
        for check in plan.executable_checks():
            result = self._run_check(plan, check)
            results.append(result)
            self.repair_loop.record_result(result)
            if not self.repair_loop.can_continue():
                results.append(self._budget_result(check))
                break
        self._results[plan.id] = results
        assessment = self.assessor.assess(plan=plan, results=results)
        self._assessments[plan.id] = assessment
        self._record_trace(
            "assessment",
            {
                "verification_plan_id": plan.id,
                "transaction_id": plan.transaction_id,
                "changeset_id": plan.changeset_id,
                "duration_ms": int((time.perf_counter() - started) * 1000),
                "completion_assessment": assessment.to_dict(),
            },
        )
        return self._observation(plan, results, assessment)

    def rerun_check(self, *, plan_id: str, check_id: str) -> dict[str, Any]:
        plan = self._plan(plan_id)
        check = next((candidate for candidate in plan.all_checks() if candidate.id == check_id), None)
        if check is None:
            raise KeyError(f"Unknown verification check: {check_id}")
        result = self._run_check(plan, check)
        existing = [item for item in self._results.get(plan.id, []) if item.check_id != check_id]
        existing.append(result)
        self._results[plan.id] = existing
        assessment = self.assessor.assess(plan=plan, results=existing)
        self._assessments[plan.id] = assessment
        return self._observation(plan, existing, assessment)

    def get_result(self, plan_id: str | None = None) -> dict[str, Any]:
        plan = self._plan(plan_id)
        results = self._results.get(plan.id, [])
        assessment = self._assessments.get(plan.id)
        if assessment is None:
            assessment = self.assessor.assess(plan=plan, results=results)
        return self._observation(plan, results, assessment)

    def _build_plan(
        self,
        *,
        profile: ProjectProfile,
        impact: ImpactAnalysis,
        transaction_id: str | None,
        changeset_id: str | None,
    ) -> VerificationPlan:
        required: list[VerificationCheck] = []
        optional: list[VerificationCheck] = []
        skipped: list[VerificationCheck] = []
        blocked: list[VerificationCheck] = []

        docs_only = (
            bool(impact.changed_files)
            and impact.risk_level == "low"
            and "Only documentation-like files changed." in impact.risk_reasons
        )
        python_sources = [
            path for path in impact.changed_files if path.endswith(".py")
        ]
        if python_sources:
            required.append(
                self._check(
                    kind=CheckKind.SYNTAX,
                    command=CommandRequest(
                        argv=[sys.executable, "-m", "py_compile", *python_sources],
                        cwd=".",
                        purpose=CommandPurpose.PROJECT_VERIFICATION,
                        timeout_seconds=60,
                    ),
                    scope="changed_python_files",
                    required=True,
                    source="impact:python-source",
                )
            )

        if docs_only:
            skipped.append(
                self._check(
                    kind=CheckKind.UNIT_TEST,
                    command=None,
                    scope="docs_only",
                    required=False,
                    source="impact:docs-only",
                    skip_reason="Only documentation files changed; unit tests are not required.",
                )
            )
            skipped.append(
                self._check(
                    kind=CheckKind.MANUAL_REVIEW,
                    command=None,
                    scope="docs_only",
                    required=True,
                    source="impact:docs-only",
                    skip_reason="Documentation correctness requires human review.",
                )
            )
        else:
            unit_command = self._command_for(profile, CheckKind.UNIT_TEST)
            if unit_command is not None:
                required.append(self._check_from_command(unit_command, required=True, scope="project"))
            else:
                blocked.append(
                    self._check(
                        kind=CheckKind.UNIT_TEST,
                        command=None,
                        scope="project",
                        required=True,
                        source="impact:source-change",
                        skip_reason="No unit test command was discovered.",
                    )
                )

        lint_command = self._command_for(profile, CheckKind.LINT)
        if lint_command is not None and not docs_only:
            required.append(self._check_from_command(lint_command, required=True, scope="project"))
        elif not docs_only:
            skipped.append(
                self._check(
                    kind=CheckKind.LINT,
                    command=None,
                    scope="project",
                    required=False,
                    source="discovery",
                    skip_reason="No lint command was discovered.",
                )
            )

        typecheck_command = self._command_for(profile, CheckKind.TYPECHECK)
        if impact.requires_typecheck:
            if typecheck_command is not None:
                required.append(self._check_from_command(typecheck_command, required=True, scope="project"))
            else:
                blocked.append(
                    self._check(
                        kind=CheckKind.TYPECHECK,
                        command=None,
                        scope="project",
                        required=True,
                        source="impact:typecheck-required",
                        skip_reason="Typecheck is required but no command was discovered.",
                    )
                )
        elif typecheck_command is not None:
            optional.append(self._check_from_command(typecheck_command, required=False, scope="project"))

        build_command = self._command_for(profile, CheckKind.BUILD)
        if impact.requires_build:
            if build_command is not None:
                required.append(self._check_from_command(build_command, required=True, scope="project"))
            else:
                blocked.append(
                    self._check(
                        kind=CheckKind.BUILD,
                        command=None,
                        scope="project",
                        required=True,
                        source="impact:build-required",
                        skip_reason="Build is required but no build command was discovered.",
                    )
                )
        elif build_command is not None and not docs_only:
            optional.append(self._check_from_command(build_command, required=False, scope="project"))

        format_command = self._command_for(profile, CheckKind.FORMAT)
        if format_command is not None and not docs_only:
            optional.append(self._check_from_command(format_command, required=False, scope="project"))

        if impact.requires_manual_review:
            skipped.append(
                self._check(
                    kind=CheckKind.MANUAL_REVIEW,
                    command=None,
                    scope="high_risk_files",
                    required=True,
                    source="impact:manual-review",
                    skip_reason="High-risk files require manual review.",
                )
            )

        required, optional, blocked = self._apply_policy(required, optional, blocked)
        return VerificationPlan(
            project_profile=profile,
            impact_analysis=impact,
            required_checks=required,
            optional_checks=optional,
            skipped_checks=skipped,
            blocked_checks=blocked,
            transaction_id=transaction_id,
            changeset_id=changeset_id,
        )

    def _apply_policy(
        self,
        required: list[VerificationCheck],
        optional: list[VerificationCheck],
        blocked: list[VerificationCheck],
    ) -> tuple[list[VerificationCheck], list[VerificationCheck], list[VerificationCheck]]:
        allowed_required: list[VerificationCheck] = []
        allowed_optional: list[VerificationCheck] = []
        blocked_checks = list(blocked)
        for collection, destination in (
            (required, allowed_required),
            (optional, allowed_optional),
        ):
            for check in collection:
                decision = self.policy.evaluate(check, workspace_root=self.workspace_root)
                check.policy_decision = decision.decision
                check.policy_reasons = decision.reasons
                check.risk_tags = decision.risk_tags
                if decision.decision == VerificationDecision.ALLOW:
                    destination.append(check)
                else:
                    check.skip_reason = "; ".join(decision.reasons)
                    blocked_checks.append(check)
        return allowed_required, allowed_optional, blocked_checks

    def _run_check(self, plan: VerificationPlan, check: VerificationCheck) -> VerificationResult:
        if check.command is None:
            return self._blocked_result(check)
        command_result = self.command_runtime.run(
            check.command,
            transaction_id=plan.transaction_id,
        )
        result = self._result_from_command(plan, check, command_result)
        attempts = [result.evidence]
        if (
            result.status == CheckStatus.FAILED
            and check.kind in {CheckKind.UNIT_TEST, CheckKind.INTEGRATION_TEST}
            and check.failure_policy == "rerun_on_flaky"
        ):
            rerun = self.command_runtime.run(
                check.command,
                transaction_id=plan.transaction_id,
            )
            rerun_result = self._result_from_command(plan, check, rerun)
            attempts.append(rerun_result.evidence)
            if result.status != rerun_result.status:
                result = VerificationResult(
                    check_id=check.id,
                    kind=check.kind,
                    status=CheckStatus.FLAKY,
                    failure_type=FailureType.FLAKY_FAILURE,
                    evidence=rerun_result.evidence,
                    repair_hints=rerun_result.repair_hints or result.repair_hints,
                    confidence_impact=-0.25,
                    duration_ms=result.duration_ms + rerun_result.duration_ms,
                    attempts=attempts,
                    policy_decision=rerun_result.policy_decision,
                )
            else:
                result = VerificationResult(
                    check_id=result.check_id,
                    kind=result.kind,
                    status=result.status,
                    failure_type=result.failure_type,
                    evidence=result.evidence,
                    repair_hints=result.repair_hints,
                    confidence_impact=result.confidence_impact,
                    duration_ms=result.duration_ms + rerun_result.duration_ms,
                    attempts=attempts,
                    policy_decision=result.policy_decision,
                )
        else:
            result = VerificationResult(
                check_id=result.check_id,
                kind=result.kind,
                status=result.status,
                failure_type=result.failure_type,
                evidence=result.evidence,
                repair_hints=result.repair_hints,
                confidence_impact=result.confidence_impact,
                duration_ms=result.duration_ms,
                attempts=attempts,
                policy_decision=result.policy_decision,
            )
        self._record_result_trace(plan, check, result)
        return result

    def _result_from_command(
        self,
        plan: VerificationPlan,
        check: VerificationCheck,
        command_result: Any,
    ) -> VerificationResult:
        output = command_result.combined_output_preview or command_result.stderr_preview or command_result.stdout_preview
        parsed = [] if command_result.semantic_status == SemanticStatus.SUCCEEDED else self.parsers.parse(output)
        failure_type = classify_failure(
            check_kind=check.kind,
            command_result=command_result,
            parsed_failures=parsed,
        )
        status = self._status_from_command(command_result, failure_type)
        evidence = VerificationEvidence(
            command_id=command_result.command_id,
            command=check.command.display_command() if check.command else None,
            exit_code=command_result.exit_code,
            output_excerpt=_excerpt(output),
            artifact_path=command_result.artifact_path,
            parsed_failures=parsed,
            duration_ms=command_result.duration_ms,
            timestamp=_now(),
        )
        repair_hints = (
            []
            if status == CheckStatus.PASSED
            else self.hints.generate(
                parsed_failures=parsed,
                failure_type=failure_type,
                changed_files=plan.impact_analysis.changed_files,
                task_intent="; ".join(plan.impact_analysis.risk_reasons),
            )
        )
        return VerificationResult(
            check_id=check.id,
            kind=check.kind,
            status=status,
            failure_type=failure_type,
            evidence=evidence,
            repair_hints=repair_hints,
            confidence_impact=0.0 if status == CheckStatus.PASSED else -0.2,
            duration_ms=command_result.duration_ms,
            policy_decision=command_result.policy_decision,
        )

    @staticmethod
    def _status_from_command(command_result: Any, failure_type: FailureType | None) -> CheckStatus:
        if command_result.semantic_status == SemanticStatus.SUCCEEDED and command_result.execution_status == ExecutionStatus.COMPLETED:
            return CheckStatus.PASSED
        if failure_type == FailureType.TIMEOUT:
            return CheckStatus.TIMEOUT
        if failure_type in {
            FailureType.CHECK_BLOCKED,
            FailureType.CHECK_REVIEW_REQUIRED,
            FailureType.CHECK_POLICY_DENIED,
            FailureType.MISSING_COMMAND,
            FailureType.ENVIRONMENT_ERROR,
        }:
            return CheckStatus.BLOCKED
        return CheckStatus.FAILED

    def _skipped_result(self, check: VerificationCheck) -> VerificationResult:
        evidence = VerificationEvidence(
            command_id=None,
            command=None,
            exit_code=None,
            output_excerpt=check.skip_reason or "Check skipped.",
            artifact_path=None,
            parsed_failures=[],
            duration_ms=0,
            timestamp=_now(),
        )
        return VerificationResult(
            check_id=check.id,
            kind=check.kind,
            status=CheckStatus.SKIPPED,
            failure_type=None if not check.required else FailureType.INCONCLUSIVE_RESULT,
            evidence=evidence,
            repair_hints=[],
            confidence_impact=-0.1 if check.required else 0.0,
            duration_ms=0,
        )

    def _blocked_result(self, check: VerificationCheck) -> VerificationResult:
        evidence = VerificationEvidence(
            command_id=None,
            command=check.command.display_command() if check.command else None,
            exit_code=None,
            output_excerpt=check.skip_reason or "Check blocked.",
            artifact_path=None,
            parsed_failures=[],
            duration_ms=0,
            timestamp=_now(),
        )
        return VerificationResult(
            check_id=check.id,
            kind=check.kind,
            status=CheckStatus.BLOCKED,
            failure_type=FailureType.CHECK_BLOCKED,
            evidence=evidence,
            repair_hints=self.hints.generate(
                parsed_failures=[],
                failure_type=FailureType.CHECK_BLOCKED,
                changed_files=[],
                task_intent=check.skip_reason or "blocked check",
            ),
            confidence_impact=-0.25,
            duration_ms=0,
        )

    def _budget_result(self, check: VerificationCheck) -> VerificationResult:
        evidence = VerificationEvidence(
            command_id=None,
            command=None,
            exit_code=None,
            output_excerpt=self.repair_loop.state.blocked_reason or "Repair budget exceeded.",
            artifact_path=None,
            parsed_failures=[],
            duration_ms=0,
            timestamp=_now(),
        )
        return VerificationResult(
            check_id=check.id,
            kind=check.kind,
            status=CheckStatus.BLOCKED,
            failure_type=FailureType.REPAIR_BUDGET_EXCEEDED,
            evidence=evidence,
            repair_hints=[],
            confidence_impact=-0.4,
            duration_ms=0,
        )

    def _check_from_command(
        self,
        command: Any,
        *,
        required: bool,
        scope: str,
    ) -> VerificationCheck:
        return self._check(
            kind=command.kind,
            command=command.request,
            scope=scope,
            required=required,
            source=command.source,
            risk_tags=[command.kind.value, command.name],
        )

    @staticmethod
    def _check(
        *,
        kind: CheckKind,
        command: CommandRequest | None,
        scope: str,
        required: bool,
        source: str,
        skip_reason: str | None = None,
        risk_tags: list[str] | None = None,
    ) -> VerificationCheck:
        timeout = command.resource_limits.timeout_seconds if command else 0.0
        return VerificationCheck(
            kind=kind,
            command=command,
            scope=scope,
            required=required,
            timeout=timeout,
            risk_tags=risk_tags or [kind.value],
            failure_policy=(
                "rerun_on_flaky"
                if kind in {CheckKind.UNIT_TEST, CheckKind.INTEGRATION_TEST}
                else "fail_fast"
            ),
            source=source,
            skip_reason=skip_reason,
        )

    @staticmethod
    def _command_for(profile: ProjectProfile, kind: CheckKind) -> Any | None:
        candidates = [command for command in profile.available_commands if command.kind == kind]
        if not candidates:
            return None
        return sorted(candidates, key=lambda command: command.confidence, reverse=True)[0]

    def _plan(self, plan_id: str | None) -> VerificationPlan:
        resolved = plan_id or self._latest_plan_id
        if resolved is None or resolved not in self._plans:
            raise KeyError("No verification plan is available.")
        return self._plans[resolved]

    def _record_result_trace(
        self,
        plan: VerificationPlan,
        check: VerificationCheck,
        result: VerificationResult,
    ) -> None:
        self._record_trace(
            "result",
            {
                "verification_plan_id": plan.id,
                "verification_check_id": check.id,
                "transaction_id": plan.transaction_id,
                "changeset_id": plan.changeset_id,
                "project_profile": plan.project_profile.to_dict(),
                "impact_analysis": plan.impact_analysis.to_dict(),
                "command_id": result.evidence.command_id,
                "policy_decision": (
                    check.policy_decision.value if check.policy_decision else None
                ),
                "check_kind": check.kind.value,
                "status": result.status.value,
                "failure_type": result.failure_type.value if result.failure_type else None,
                "parsed_failures": [
                    failure.to_dict() for failure in result.evidence.parsed_failures
                ],
                "evidence_artifact": result.evidence.artifact_path,
                "duration_ms": result.duration_ms,
                "confidence_impact": result.confidence_impact,
                "repair_hints": [hint.to_dict() for hint in result.repair_hints],
            },
        )

    def _record_trace(self, phase: str, data: dict[str, Any]) -> None:
        if self.trace is None:
            return
        payload = dict(data)
        payload["phase"] = phase
        self.trace.record("verification", payload)

    @staticmethod
    def _observation(
        plan: VerificationPlan,
        results: list[VerificationResult],
        assessment: CompletionAssessment,
    ) -> dict[str, Any]:
        failed = [
            result
            for result in results
            if result.status
            in {CheckStatus.FAILED, CheckStatus.BLOCKED, CheckStatus.TIMEOUT, CheckStatus.FLAKY}
        ]
        return {
            "verification": {
                "plan": plan.to_dict(),
                "check_status": [
                    {
                        "check_id": result.check_id,
                        "kind": result.kind.value,
                        "status": result.status.value,
                        "failure_type": result.failure_type.value if result.failure_type else None,
                    }
                    for result in results
                ],
                "failed_checks": [result.to_dict() for result in failed],
                "repair_hints": [
                    hint.to_dict()
                    for result in failed
                    for hint in result.repair_hints
                ],
                "completion_assessment": assessment.to_dict(),
            }
        }


def _excerpt(output: str, limit: int = 1200) -> str:
    if len(output) <= limit:
        return output
    marker = "\n...[truncated]...\n"
    head = (limit - len(marker)) // 2
    tail = limit - len(marker) - head
    return f"{output[:head]}{marker}{output[-tail:]}"


def _now() -> str:
    return datetime.now(UTC).isoformat()
