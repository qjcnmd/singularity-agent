from __future__ import annotations

from miniharness.verification.models import (
    FailureType,
    ParsedFailure,
    RepairHint,
    RepairLoopState,
    VerificationResult,
)


class RepairHintGenerator:
    def generate(
        self,
        *,
        parsed_failures: list[ParsedFailure],
        failure_type: FailureType | None,
        changed_files: list[str],
        task_intent: str,
    ) -> list[RepairHint]:
        hints: list[RepairHint] = []
        for failure in parsed_failures[:5]:
            target_file = failure.file or self._fallback_file(changed_files)
            next_action = (
                f"Inspect {target_file}"
                if target_file
                else "Inspect the verification output artifact"
            )
            if failure.line:
                next_action += f" around line {failure.line}"
            if failure.test_name:
                next_action += f" and rerun {failure.test_name}"
            hints.append(
                RepairHint(
                    target_file=target_file,
                    line=failure.line,
                    test_name=failure.test_name,
                    message=failure.message,
                    next_action=next_action,
                    confidence=0.8 if failure.file else 0.55,
                )
            )
        if hints:
            return hints

        message = (
            f"Verification failed with {failure_type.value}."
            if failure_type
            else "Verification result is inconclusive."
        )
        target = self._fallback_file(changed_files)
        return [
            RepairHint(
                target_file=target,
                line=None,
                test_name=None,
                message=message,
                next_action=(
                    f"Review recent changes in {target} for intent: {task_intent}."
                    if target
                    else "Review command output and rerun the smallest relevant check."
                ),
                confidence=0.4,
            )
        ]

    @staticmethod
    def _fallback_file(changed_files: list[str]) -> str | None:
        for path in changed_files:
            if not path.lower().endswith((".md", ".txt", ".rst")):
                return path
        return changed_files[0] if changed_files else None


class RepairLoopController:
    def __init__(self, state: RepairLoopState | None = None) -> None:
        self.state = state or RepairLoopState()

    def record_result(self, result: VerificationResult) -> RepairLoopState:
        self.state.total_commands += max(1, len(result.attempts) or 1)
        self.state.total_time_seconds += result.duration_ms / 1000
        if result.failure_type is not None:
            fingerprint = self._fingerprint(result)
            count = self.state.failure_fingerprints.get(fingerprint, 0) + 1
            self.state.failure_fingerprints[fingerprint] = count
            if count > self.state.budget.max_same_failure_retries:
                self.state.blocked_reason = "same_failure_retry_budget_exceeded"
        if self.state.total_commands > self.state.budget.max_total_commands:
            self.state.blocked_reason = "command_budget_exceeded"
        if self.state.total_time_seconds > self.state.budget.max_total_time_seconds:
            self.state.blocked_reason = "time_budget_exceeded"
        return self.state

    def can_continue(self) -> bool:
        return self.state.blocked_reason is None

    @staticmethod
    def _fingerprint(result: VerificationResult) -> str:
        first_failure = result.evidence.parsed_failures[0] if result.evidence.parsed_failures else None
        if first_failure is None:
            return f"{result.check_id}:{result.failure_type}"
        return ":".join(
            [
                result.check_id,
                result.failure_type.value if result.failure_type else "unknown",
                first_failure.file or "",
                str(first_failure.line or ""),
                first_failure.test_name or "",
                first_failure.message[:120],
            ]
        )
