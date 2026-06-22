from __future__ import annotations

from pathlib import Path
from typing import Any

from singularity.edit.apply import EditApplier
from singularity.edit.models import (
    EditFailureCategory,
    EditIntent,
    EditIssue,
    EditIssueSeverity,
    EditResult,
    PatchValidationResult,
)
from singularity.edit.patch import PatchBuildError, PatchBuilder
from singularity.edit.planner import EditPlanBuilder
from singularity.edit.repair import EditRepairController
from singularity.edit.validation import PatchValidator
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.review.models import ReviewDecisionAction
from singularity.workspace import MutationError, MutationRuntime


class EditRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        mutation_runtime: MutationRuntime | None = None,
        project_index_runtime: Any | None = None,
        verification_runtime: Any | None = None,
        trace: Any | None = None,
        planner: Any | None = None,
        context_manager: Any | None = None,
        review_runtime: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.mutation_runtime = mutation_runtime or MutationRuntime(self.workspace_root)
        self.project_index_runtime = project_index_runtime
        self.verification_runtime = verification_runtime
        self.trace = trace
        self.planner = planner
        self.context_manager = context_manager
        self.review_runtime = review_runtime
        self.plan_builder = EditPlanBuilder()
        self.patch_builder = PatchBuilder(self.workspace_root)
        self.validator = PatchValidator(
            self.workspace_root,
            mutation_runtime=self.mutation_runtime,
            project_index_runtime=project_index_runtime,
        )
        self.repair = EditRepairController(self.mutation_runtime)
        self.applier = EditApplier(self.mutation_runtime)

    def plan_intent(self, intent: EditIntent) -> EditResult:
        self._throw_if_cancelled()
        plan = self.plan_builder.build(intent)
        self._emit(
            TraceEventType.EDIT_PLAN_CREATED,
            f"Edit plan created with {plan.strategy.value}.",
            {
                "edit_plan_id": plan.id,
                "intent_id": intent.id,
                "strategy": plan.strategy.value,
                "operation_count": len(plan.operations),
                "paths": intent.paths,
                "rationale": plan.rationale,
            },
        )
        result = EditResult(
            ok=True,
            status="planned",
            intent_id=intent.id,
            plan=plan,
            changed_files=intent.paths,
            message="Edit plan created.",
        )
        self._record_context(result)
        return result

    def preview_intent(self, intent: EditIntent, *, tool_call_id: str | None = None) -> EditResult:
        return self._run(intent, apply=False, tool_call_id=tool_call_id, repair=False)

    def apply_intent(self, intent: EditIntent, *, tool_call_id: str | None = None) -> EditResult:
        return self._run(intent, apply=True, tool_call_id=tool_call_id, repair=True)

    def write_file(
        self,
        *,
        path: str,
        content: str,
        mode: str,
        encoding: str = "utf-8",
        create_dirs: bool = False,
        reason: str | None = None,
        tool_call_id: str | None = None,
    ) -> Any:
        self._throw_if_cancelled()
        if encoding.lower().replace("_", "-") != "utf-8":
            raise MutationError(
                "unsupported_operation",
                "write_file only supports utf-8 text in this phase.",
                {"encoding": encoding},
            )
        resolved = self.mutation_runtime.resolver.resolve(path)
        if not create_dirs and not resolved.path.parent.exists():
            raise MutationError(
                "parent_directory_missing",
                f"Parent directory does not exist: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        exists = resolved.path.exists()
        if mode == "create" and exists:
            raise MutationError(
                "invalid_operation",
                f"Cannot create file that already exists: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        if mode == "overwrite" and not exists:
            raise MutationError(
                "file_not_found",
                f"Cannot overwrite missing file: {resolved.relative_posix}",
                {"path": resolved.relative_posix},
            )
        result = self.mutation_runtime.apply_file_updates(
            {resolved.relative_posix: content},
            intent=reason or f"write_file {mode}",
            created_by="edit_runtime",
            tool_call_id=tool_call_id,
        )
        self._emit(
            TraceEventType.EDIT_APPLIED if result.ok else TraceEventType.EDIT_FAILED,
            "write_file facade delegated through EditRuntime.",
            {
                "path": resolved.relative_posix,
                "mode": mode,
                "create_dirs": create_dirs,
                "changeset_id": result.changeset_id,
                "transaction_id": result.transaction_id,
                "status": result.status,
                "error_code": result.error_code,
            },
            severity=TraceSeverity.INFO if result.ok else TraceSeverity.WARNING,
        )
        return result

    def apply_unified_diff(
        self,
        *,
        patch: str,
        reason: str | None = None,
        expected_files: list[str] | None = None,
        allow_new_files: bool = True,
        tool_call_id: str | None = None,
    ) -> Any:
        self._throw_if_cancelled()
        operations = self.mutation_runtime.operations_from_unified_diff(
            patch,
            expected_files=expected_files,
            allow_new_files=allow_new_files,
        )
        result = self.applier.apply(
            operations,
            intent=reason or "apply unified diff",
            tool_call_id=tool_call_id,
        )
        self._emit(
            TraceEventType.EDIT_APPLIED if result.ok else TraceEventType.EDIT_FAILED,
            "apply_patch facade delegated through EditRuntime.",
            {
                "changed_files": result.affected_files,
                "changeset_id": result.changeset_id,
                "transaction_id": result.transaction_id,
                "status": result.status,
                "error_code": result.error_code,
            },
            severity=TraceSeverity.INFO if result.ok else TraceSeverity.WARNING,
        )
        return result

    def _run(
        self,
        intent: EditIntent,
        *,
        apply: bool,
        tool_call_id: str | None,
        repair: bool,
    ) -> EditResult:
        self._throw_if_cancelled()
        attempts = 0
        candidates_used = 0
        repair_attempts = []
        current_intent = intent
        last_result: EditResult | None = None

        while True:
            result = self._single_pass(current_intent, apply=apply, tool_call_id=tool_call_id)
            result.repair_attempts = [*repair_attempts, *result.repair_attempts]
            candidates_used += 1
            if result.ok or not repair:
                self._record_context(result)
                return result
            category = self._failure_category(result)
            if (
                attempts >= current_intent.scope.max_repair_attempts
                or candidates_used >= current_intent.scope.max_candidates
                or not self.repair.can_repair(category)
            ):
                if not self.repair.can_repair(category):
                    self._emit(
                        TraceEventType.EDIT_FAILED,
                        "Edit failed without repair because the category is not recoverable.",
                        self._trace_payload(result),
                        severity=TraceSeverity.WARNING,
                    )
                elif attempts >= current_intent.scope.max_repair_attempts:
                    result.error_code = result.error_code or "edit_repair_budget_exceeded"
                    result.message = result.message or "Edit repair budget exceeded."
                self._record_context(result)
                return result
            attempts += 1
            repaired, attempt = self.repair.repair_intent(
                current_intent,
                category=category,
                attempt_number=attempts,
            )
            repair_attempts.append(attempt)
            self._emit(
                TraceEventType.EDIT_REPAIR_ATTEMPTED,
                f"Edit repair attempted: {attempt.action}.",
                {"attempt": attempt.to_dict(), "intent_id": intent.id},
                severity=TraceSeverity.WARNING,
            )
            if repaired is None:
                result.repair_attempts = repair_attempts
                self._record_context(result)
                return result
            current_intent = repaired
            last_result = result

        return last_result or EditResult(ok=False, status="failed", intent_id=intent.id)

    def _single_pass(self, intent: EditIntent, *, apply: bool, tool_call_id: str | None) -> EditResult:
        plan_result = self.plan_intent(intent)
        plan = plan_result.plan
        assert plan is not None
        try:
            candidate = self.patch_builder.build(plan)
        except PatchBuildError as exc:
            validation = PatchValidationResult(
                ok=False,
                issues=[
                    EditIssue(
                        code=exc.code,
                        message=str(exc),
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.CONTEXT_MISMATCH if "context" in exc.code or "not_found" in exc.code else EditFailureCategory.INTERNAL,
                        path=exc.path,
                    )
                ],
                failure_category=EditFailureCategory.CONTEXT_MISMATCH if "context" in exc.code or "not_found" in exc.code else EditFailureCategory.INTERNAL,
            )
            return EditResult(
                ok=False,
                status="failed",
                intent_id=intent.id,
                plan=plan,
                validation=validation,
                error_code=exc.code,
                message=str(exc),
            )

        validation = self.validator.validate(candidate, intent_summary=intent.summary, scope=intent.scope)
        self._emit(
            TraceEventType.EDIT_PATCH_VALIDATED,
            "Edit patch candidate validated.",
            {
                "edit_plan_id": plan.id,
                "patch_candidate_id": candidate.id,
                "patch_digest": candidate.digest,
                "ok": validation.ok,
                "requires_review": validation.requires_review,
                "issue_codes": [issue.code for issue in validation.issues],
                "changed_files": validation.changed_files,
                "diff_summary": validation.diff_summary,
            },
            severity=TraceSeverity.WARNING if not validation.ok else TraceSeverity.INFO,
        )
        pre_review = self._pre_edit_review(
            intent=intent,
            plan=plan,
            candidate=candidate,
            validation=validation,
        )
        if pre_review is not None and pre_review.decision.action != ReviewDecisionAction.ACCEPT:
            return self._review_blocked_result(
                intent=intent,
                plan=plan,
                candidate=candidate,
                validation=validation,
                review_report=pre_review,
                message="Pre-edit review blocked the patch.",
            )
        if not validation.ok:
            return EditResult(
                ok=False,
                status="requires_review" if validation.requires_review else "failed",
                intent_id=intent.id,
                plan=plan,
                candidate=candidate,
                validation=validation,
                changed_files=validation.changed_files,
                code_impact=validation.code_impact,
                test_impact=validation.test_impact,
                review_report=pre_review.model_dump(mode="json") if pre_review is not None else None,
                error_code=validation.failure_category.value,
                message="Edit validation failed.",
            )

        preview = self.applier.preview(
            candidate.operations,
            intent=intent.summary,
            tool_call_id=tool_call_id,
        )
        if not preview.ok:
            return EditResult(
                ok=False,
                status=preview.status,
                intent_id=intent.id,
                plan=plan,
                candidate=candidate,
                validation=validation,
                mutation_result=preview,
                changed_files=preview.affected_files,
                changeset_id=preview.changeset_id,
                code_impact=validation.code_impact,
                test_impact=validation.test_impact,
                review_report=pre_review.model_dump(mode="json") if pre_review is not None else None,
                error_code=preview.error_code,
                message=preview.message,
            )
        if not apply:
            return EditResult(
                ok=True,
                status="preview",
                intent_id=intent.id,
                plan=plan,
                candidate=candidate,
                validation=validation,
                mutation_result=preview,
                changed_files=preview.affected_files,
                changeset_id=preview.changeset_id,
                code_impact=validation.code_impact,
                test_impact=validation.test_impact,
                review_report=pre_review.model_dump(mode="json") if pre_review is not None else None,
                message="Edit preview is valid.",
            )

        mutation = self.applier.apply(
            candidate.operations,
            intent=intent.summary,
            tool_call_id=tool_call_id,
        )
        code_impact = validation.code_impact
        test_impact = validation.test_impact
        verification_plan = None
        if mutation.ok:
            code_impact, test_impact = self._post_apply_impact(mutation.affected_files)
            verification_plan = self._plan_verification(
                changed_files=mutation.affected_files,
                task_intent=intent.summary,
                transaction_id=mutation.transaction_id,
                changeset_id=mutation.changeset_id,
            )
        result = EditResult(
            ok=mutation.ok,
            status="applied" if mutation.ok else mutation.status,
            intent_id=intent.id,
            plan=plan,
            candidate=candidate,
            validation=validation,
            mutation_result=mutation,
            changed_files=mutation.affected_files,
            changeset_id=mutation.changeset_id,
            transaction_id=mutation.transaction_id,
            verification_plan=verification_plan,
            code_impact=code_impact,
            test_impact=test_impact,
            error_code=mutation.error_code,
            message=mutation.message,
        )
        if mutation.ok:
            post_review = self._post_patch_review(result)
            if post_review is not None:
                result.review_report = post_review.model_dump(mode="json")
                if post_review.decision.action != ReviewDecisionAction.ACCEPT:
                    result.ok = False
                    result.status = f"review_{post_review.decision.action.value}"
                    result.error_code = f"review_{post_review.decision.action.value}"
                    result.message = "; ".join(post_review.decision.reasons) or "Post-patch review blocked continuation."
        self._emit(
            TraceEventType.EDIT_APPLIED if mutation.ok else TraceEventType.EDIT_FAILED,
            "Edit applied through MutationRuntime." if mutation.ok else "Edit apply failed.",
            self._trace_payload(result),
            severity=TraceSeverity.INFO if mutation.ok else TraceSeverity.WARNING,
        )
        if self.planner is not None and hasattr(self.planner, "update_from_edit"):
            self.planner.update_from_edit({"edit": result.to_dict()}, tool_call_id=tool_call_id)
        return result

    def _pre_edit_review(
        self,
        *,
        intent: EditIntent,
        plan: Any,
        candidate: Any,
        validation: PatchValidationResult,
    ) -> Any | None:
        if self.review_runtime is None or not hasattr(self.review_runtime, "pre_edit_review"):
            return None
        return self.review_runtime.pre_edit_review(
            intent=intent,
            plan=plan,
            patch=candidate,
            validation=validation,
            code_impact=validation.code_impact,
            test_impact=validation.test_impact,
            task_id=getattr(self.planner, "task_id", None),
            plan_id=getattr(getattr(self.planner, "plan", None), "plan_id", None),
        )

    def _post_patch_review(self, result: EditResult) -> Any | None:
        if self.review_runtime is None or not hasattr(self.review_runtime, "post_patch_review"):
            return None
        return self.review_runtime.post_patch_review(
            edit_result=result.to_dict(),
            mutation_result=result.mutation_result,
            verification_plan=result.verification_plan,
            code_impact=result.code_impact,
            test_impact=result.test_impact,
        )

    @staticmethod
    def _review_blocked_result(
        *,
        intent: EditIntent,
        plan: Any,
        candidate: Any,
        validation: PatchValidationResult,
        review_report: Any,
        message: str,
    ) -> EditResult:
        action = review_report.decision.action.value
        return EditResult(
            ok=False,
            status=f"review_{action}",
            intent_id=intent.id,
            plan=plan,
            candidate=candidate,
            validation=validation,
            changed_files=validation.changed_files,
            code_impact=validation.code_impact,
            test_impact=validation.test_impact,
            review_report=review_report.model_dump(mode="json"),
            error_code=f"review_{action}",
            message="; ".join(review_report.decision.reasons) or message,
        )

    def _plan_verification(
        self,
        *,
        changed_files: list[str],
        task_intent: str,
        transaction_id: str | None,
        changeset_id: str | None,
    ) -> dict[str, Any] | None:
        if self.verification_runtime is None or not changed_files:
            return None
        plan = self.verification_runtime.plan_verification(
            changed_files=changed_files,
            task_intent=task_intent,
            transaction_id=transaction_id,
            changeset_id=changeset_id,
        )
        return plan.to_dict() if hasattr(plan, "to_dict") else dict(plan)

    def _post_apply_impact(self, changed_files: list[str]) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
        if self.project_index_runtime is None or not changed_files:
            return None, None
        try:
            impact = self.project_index_runtime.analyze_impact(changed_files)
            tests = self.project_index_runtime.get_test_impact(changed_files)
        except Exception:
            return None, None
        return (
            impact.to_dict() if hasattr(impact, "to_dict") else dict(impact),
            tests.to_dict() if hasattr(tests, "to_dict") else dict(tests),
        )

    def _record_context(self, result: EditResult) -> None:
        if self.context_manager is not None and hasattr(self.context_manager, "add_edit_result"):
            self.context_manager.add_edit_result(result.to_dict())

    def _failure_category(self, result: EditResult) -> EditFailureCategory:
        if result.error_code and str(result.error_code).startswith("review_"):
            return EditFailureCategory.REVIEW_REQUIRED
        if result.validation is not None:
            return EditFailureCategory(result.validation.failure_category)
        if result.error_code in {"snapshot_mismatch", "file_changed"}:
            return EditFailureCategory.FRESHNESS
        if result.error_code in {"patch_context_not_found", "patch_context_ambiguous"}:
            return EditFailureCategory.CONTEXT_MISMATCH
        return EditFailureCategory.MUTATION_FAILED

    def _trace_payload(self, result: EditResult) -> dict[str, Any]:
        payload = result.to_dict()
        payload.pop("mutation_observation", None)
        return payload

    def _emit(
        self,
        event_type: TraceEventType,
        summary: str,
        payload: dict[str, Any],
        *,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None:
            return
        if hasattr(self.trace, "emit"):
            self.trace.emit(
                event_type,
                runtime="edit",
                summary=summary,
                payload=payload,
                ids={
                    "session_id": getattr(self.planner, "session_id", None),
                    "task_id": getattr(self.planner, "task_id", None),
                    "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None),
                },
                severity=severity,
            )
        elif hasattr(self.trace, "record"):
            self.trace.record("edit", {"event_type": event_type.value, "summary": summary, **payload})

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()
