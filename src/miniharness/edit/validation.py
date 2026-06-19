from __future__ import annotations

import ast
import json
import tomllib
from pathlib import Path
from typing import Any

from miniharness.edit.models import (
    EditFailureCategory,
    EditIssue,
    EditIssueSeverity,
    EditStrategyKind,
    PatchCandidate,
    PatchValidationResult,
)
from miniharness.workspace.errors import MutationError
from miniharness.workspace.policy import DENY, REQUIRE_REVIEW


class PatchValidator:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        mutation_runtime: Any,
        project_index_runtime: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.mutation_runtime = mutation_runtime
        self.project_index_runtime = project_index_runtime

    def validate(self, candidate: PatchCandidate, *, intent_summary: str, scope: Any) -> PatchValidationResult:
        issues: list[EditIssue] = []
        issues.extend(self._validate_path_scope(candidate, scope))
        if _blocking(issues):
            return self._result(False, issues, candidate=candidate)

        try:
            changeset = self.mutation_runtime.create_changeset(
                candidate.operations,
                intent=intent_summary,
                created_by="edit_runtime",
            )
        except MutationError as exc:
            issue = _issue_from_mutation_error(exc)
            return self._result(False, [issue], candidate=candidate)

        issues.extend(self._validate_policy(changeset))
        issues.extend(self._validate_diff_budget(candidate, changeset, scope))
        issues.extend(self._validate_final_texts(changeset))
        code_impact, test_impact, impact_issues = self._impact(changeset.affected_files)
        issues.extend(impact_issues)

        has_errors = any(issue.severity == EditIssueSeverity.ERROR for issue in issues)
        requires_review = any(issue.severity == EditIssueSeverity.REVIEW for issue in issues)
        category = _first_category(issues, default=EditFailureCategory.NONE)
        return PatchValidationResult(
            ok=not has_errors and not requires_review,
            requires_review=requires_review,
            issues=issues,
            changed_files=changeset.affected_files,
            diff_summary=[diff.summary() for diff in changeset.diffs],
            code_impact=code_impact,
            test_impact=test_impact,
            changeset_id=changeset.id,
            failure_category=category,
            changeset=changeset,
        )

    def _validate_path_scope(self, candidate: PatchCandidate, scope: Any) -> list[EditIssue]:
        issues: list[EditIssue] = []
        if len(candidate.touched_paths) > int(scope.max_files):
            issues.append(
                EditIssue(
                    code="edit_scope_too_large",
                    message=f"Edit touches {len(candidate.touched_paths)} files; limit is {scope.max_files}.",
                    severity=EditIssueSeverity.ERROR,
                    category=EditFailureCategory.PATH_SCOPE,
                )
            )
        allowed_roots = [_normalize(path) for path in scope.paths]
        excluded_roots = [_normalize(path) for path in scope.exclude_paths]
        for path in candidate.touched_paths:
            normalized = _normalize(path)
            try:
                resolved = (self.workspace_root / normalized).resolve(strict=False)
                resolved.relative_to(self.workspace_root)
            except ValueError:
                issues.append(
                    EditIssue(
                        code="path_outside_workspace",
                        message="Edit path is outside workspace.",
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.PATH_SCOPE,
                        path=path,
                    )
                )
            if allowed_roots and not any(_is_under(normalized, root) for root in allowed_roots):
                issues.append(
                    EditIssue(
                        code="path_outside_edit_scope",
                        message="Edit path is outside requested edit scope.",
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.PATH_SCOPE,
                        path=path,
                    )
                )
            if any(_is_under(normalized, root) for root in excluded_roots):
                issues.append(
                    EditIssue(
                        code="path_excluded_from_edit_scope",
                        message="Edit path is excluded from requested edit scope.",
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.PATH_SCOPE,
                        path=path,
                    )
                )
            expected = scope.expected_hashes.get(path) or scope.expected_hashes.get(normalized)
            if expected:
                current = self.mutation_runtime.index.current_hash(normalized)
                if current != expected:
                    issues.append(
                        EditIssue(
                            code="expected_hash_stale",
                            message="Expected file hash does not match current file.",
                            severity=EditIssueSeverity.ERROR,
                            category=EditFailureCategory.FRESHNESS,
                            path=path,
                            details={"expected_sha256": expected, "current_sha256": current},
                        )
                    )
        return issues

    @staticmethod
    def _validate_policy(changeset: Any) -> list[EditIssue]:
        issues: list[EditIssue] = []
        for decision in changeset.policy_decisions:
            if decision.decision == DENY:
                issues.append(
                    EditIssue(
                        code=decision.error_code or "policy_denied",
                        message="Workspace policy denied this edit.",
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.POLICY_DENIED,
                        details={
                            "file_class": decision.file_class,
                            "reasons": decision.reasons,
                            "risk_tags": decision.risk_tags,
                        },
                    )
                )
            elif decision.decision == REQUIRE_REVIEW:
                issues.append(
                    EditIssue(
                        code=decision.error_code or "review_required",
                        message="Workspace policy requires review for this edit.",
                        severity=EditIssueSeverity.REVIEW,
                        category=EditFailureCategory.REVIEW_REQUIRED,
                        details={
                            "file_class": decision.file_class,
                            "reasons": decision.reasons,
                            "risk_tags": decision.risk_tags,
                        },
                    )
                )
        return issues

    def _validate_diff_budget(self, candidate: PatchCandidate, changeset: Any, scope: Any) -> list[EditIssue]:
        issues: list[EditIssue] = []
        total_changed = sum(diff.added_lines + diff.removed_lines for diff in changeset.diffs)
        if candidate.strategy == EditStrategyKind.TARGETED_PATCH:
            if total_changed > int(scope.targeted_max_changed_lines):
                issues.append(
                    EditIssue(
                        code="targeted_patch_diff_budget_exceeded",
                        message="Targeted patch diff is too large.",
                        severity=EditIssueSeverity.REVIEW,
                        category=EditFailureCategory.OVER_MODIFICATION,
                        details={"changed_lines": total_changed, "limit": scope.targeted_max_changed_lines},
                    )
                )
            for diff in changeset.diffs:
                line_count = self._line_count(diff.path)
                if line_count and (diff.added_lines + diff.removed_lines) / line_count > float(scope.targeted_max_file_change_ratio):
                    issues.append(
                        EditIssue(
                            code="targeted_patch_file_ratio_exceeded",
                            message="Targeted patch changes too much of one file.",
                            severity=EditIssueSeverity.REVIEW,
                            category=EditFailureCategory.OVER_MODIFICATION,
                            path=diff.path,
                            details={"changed_lines": diff.added_lines + diff.removed_lines, "file_lines": line_count},
                        )
                    )
        if candidate.strategy == EditStrategyKind.FULL_FILE_REWRITE and total_changed > int(scope.rewrite_max_changed_lines):
            issues.append(
                EditIssue(
                    code="full_rewrite_diff_budget_exceeded",
                    message="Full-file rewrite exceeds review threshold.",
                    severity=EditIssueSeverity.REVIEW,
                    category=EditFailureCategory.DIFF_BUDGET,
                    details={"changed_lines": total_changed, "limit": scope.rewrite_max_changed_lines},
                )
            )
        return issues

    def _line_count(self, path: str) -> int:
        file_path = self.workspace_root / path
        if not file_path.exists() or not file_path.is_file():
            return 0
        try:
            return max(1, len(file_path.read_text(encoding="utf-8").splitlines()))
        except UnicodeDecodeError:
            return 0

    @staticmethod
    def _validate_final_texts(changeset: Any) -> list[EditIssue]:
        issues: list[EditIssue] = []
        for path, text in changeset.final_texts.items():
            if text is None:
                continue
            suffix = Path(path).suffix.lower()
            try:
                if suffix == ".py":
                    ast.parse(text, filename=path)
                elif suffix == ".json":
                    json.loads(text)
                elif suffix == ".toml":
                    tomllib.loads(text)
            except Exception as exc:
                issues.append(
                    EditIssue(
                        code="syntax_risk",
                        message=f"Edited file did not parse as {suffix or 'text'}.",
                        severity=EditIssueSeverity.ERROR,
                        category=EditFailureCategory.SYNTAX_RISK,
                        path=path,
                        details={"error": str(exc), "type": type(exc).__name__},
                    )
                )
            if text and not text.endswith(("\n", "\r\n")):
                issues.append(
                    EditIssue(
                        code="missing_final_newline",
                        message="Edited text does not end with a newline.",
                        severity=EditIssueSeverity.WARNING,
                        category=EditFailureCategory.FORMAT_RISK,
                        path=path,
                    )
                )
        return issues

    def _impact(self, changed_files: list[str]) -> tuple[dict[str, Any] | None, dict[str, Any] | None, list[EditIssue]]:
        if self.project_index_runtime is None or not changed_files:
            return None, None, []
        issues: list[EditIssue] = []
        try:
            impact_obj = self.project_index_runtime.analyze_impact(changed_files)
            test_obj = self.project_index_runtime.get_test_impact(changed_files)
            impact = impact_obj.to_dict() if hasattr(impact_obj, "to_dict") else dict(impact_obj)
            test_impact = test_obj.to_dict() if hasattr(test_obj, "to_dict") else dict(test_obj)
        except Exception as exc:
            return None, None, [
                EditIssue(
                    code="code_index_impact_failed",
                    message="CodeIndex impact analysis failed.",
                    severity=EditIssueSeverity.WARNING,
                    category=EditFailureCategory.INTERNAL,
                    details={"error": str(exc), "type": type(exc).__name__},
                )
            ]
        if impact.get("config_impact") or impact.get("generated_or_vendor_impact") or impact.get("broad_impact") or impact.get("affected_entrypoints"):
            issues.append(
                EditIssue(
                    code="code_index_high_impact",
                    message="CodeIndex marked this edit as high impact.",
                    severity=EditIssueSeverity.REVIEW,
                    category=EditFailureCategory.REVIEW_REQUIRED,
                    details={
                        "risk_level": impact.get("risk_level"),
                        "risk_reasons": impact.get("risk_reasons"),
                        "affected_entrypoints": impact.get("affected_entrypoints"),
                    },
                )
            )
        return impact, test_impact, issues

    @staticmethod
    def _result(ok: bool, issues: list[EditIssue], *, candidate: PatchCandidate) -> PatchValidationResult:
        return PatchValidationResult(
            ok=ok,
            requires_review=any(issue.severity == EditIssueSeverity.REVIEW for issue in issues),
            issues=issues,
            changed_files=candidate.touched_paths,
            failure_category=_first_category(issues),
        )


def _issue_from_mutation_error(exc: MutationError) -> EditIssue:
    category = {
        "snapshot_mismatch": EditFailureCategory.FRESHNESS,
        "file_changed": EditFailureCategory.FRESHNESS,
        "patch_context_not_found": EditFailureCategory.CONTEXT_MISMATCH,
        "patch_context_ambiguous": EditFailureCategory.CONTEXT_MISMATCH,
        "path_denied": EditFailureCategory.PATH_SCOPE,
        "file_class_denied": EditFailureCategory.POLICY_DENIED,
        "binary_file_denied": EditFailureCategory.POLICY_DENIED,
    }.get(exc.code, EditFailureCategory.MUTATION_FAILED)
    return EditIssue(
        code=exc.code,
        message=str(exc),
        severity=EditIssueSeverity.ERROR,
        category=category,
        path=(exc.details or {}).get("path") if hasattr(exc, "details") else None,
        details=dict(getattr(exc, "details", {}) or {}),
    )


def _blocking(issues: list[EditIssue]) -> bool:
    return any(issue.severity == EditIssueSeverity.ERROR for issue in issues)


def _first_category(issues: list[EditIssue], *, default: EditFailureCategory = EditFailureCategory.INTERNAL) -> EditFailureCategory:
    if not issues:
        return default
    return EditFailureCategory(issues[0].category)


def _normalize(path: str) -> str:
    return Path(path).as_posix().strip("/")


def _is_under(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip("/") + "/")

