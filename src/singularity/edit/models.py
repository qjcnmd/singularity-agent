from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import StrEnum
from typing import Any
from uuid import uuid4

from singularity.utils.serialization import coerce_enum, stable_hash_text, to_plain_data, utc_iso_timestamp


class EditStrategyKind(StrEnum):
    TARGETED_PATCH = "targeted_patch"
    FULL_FILE_REWRITE = "full_file_rewrite"
    STRUCTURED_EDIT = "structured_edit"


class EditOperationKind(StrEnum):
    REPLACE_TEXT = "replace_text"
    INSERT_BEFORE = "insert_before"
    INSERT_AFTER = "insert_after"
    REPLACE_RANGE = "replace_range"
    CREATE_FILE = "create_file"
    REWRITE_FILE = "rewrite_file"
    UPDATE_JSON = "update_json"
    REPLACE_SYMBOL = "replace_symbol"
    REPLACE_IMPORT = "replace_import"
    UNIFIED_DIFF = "unified_diff"


class EditIssueSeverity(StrEnum):
    INFO = "info"
    WARNING = "warning"
    ERROR = "error"
    REVIEW = "review"


class EditFailureCategory(StrEnum):
    NONE = "none"
    PATH_SCOPE = "path_scope"
    FRESHNESS = "freshness"
    CONTEXT_MISMATCH = "context_mismatch"
    DIFF_BUDGET = "diff_budget"
    POLICY_DENIED = "policy_denied"
    REVIEW_REQUIRED = "review_required"
    SYNTAX_RISK = "syntax_risk"
    FORMAT_RISK = "format_risk"
    OVER_MODIFICATION = "over_modification"
    MUTATION_FAILED = "mutation_failed"
    REPAIR_BUDGET = "repair_budget"
    INTERNAL = "internal"


@dataclass
class EditScope:
    paths: list[str] = field(default_factory=list)
    exclude_paths: list[str] = field(default_factory=list)
    expected_hashes: dict[str, str] = field(default_factory=dict)
    max_files: int = 20
    targeted_max_changed_lines: int = 120
    targeted_max_file_change_ratio: float = 0.25
    rewrite_max_changed_lines: int = 500
    max_repair_attempts: int = 2
    max_candidates: int = 3
    allow_create: bool = True
    allow_delete: bool = False
    allow_move: bool = False

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class EditOperation:
    kind: EditOperationKind | str
    path: str
    old_text: str | None = None
    new_text: str | None = None
    marker: str | None = None
    text: str | None = None
    start_line: int | None = None
    end_line: int | None = None
    content: str | None = None
    updates: dict[str, Any] | None = None
    symbol_name: str | None = None
    symbol_kind: str | None = None
    import_name: str | None = None
    new_path: str | None = None
    diff: str | None = None
    expected_sha256: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
    id: str = field(default_factory=lambda: f"editop_{uuid4().hex[:12]}")

    def __post_init__(self) -> None:
        self.kind = _enum(EditOperationKind, self.kind)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class EditIntent:
    summary: str
    operations: list[EditOperation]
    scope: EditScope = field(default_factory=EditScope)
    strategy: EditStrategyKind | str | None = None
    actor: str = "agent"
    id: str = field(default_factory=lambda: f"editintent_{uuid4().hex[:12]}")
    created_at: str = field(default_factory=lambda: _now())
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.operations = [
            operation if isinstance(operation, EditOperation) else EditOperation(**dict(operation))
            for operation in self.operations
        ]
        if not isinstance(self.scope, EditScope):
            self.scope = EditScope(**dict(self.scope))
        if self.strategy is not None:
            self.strategy = _enum(EditStrategyKind, self.strategy)

    @property
    def paths(self) -> list[str]:
        paths: list[str] = []
        for operation in self.operations:
            paths.append(operation.path)
            if operation.new_path:
                paths.append(operation.new_path)
        return sorted(set(paths))

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class EditIssue:
    code: str
    message: str
    severity: EditIssueSeverity | str = EditIssueSeverity.ERROR
    category: EditFailureCategory | str = EditFailureCategory.INTERNAL
    path: str | None = None
    details: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.severity = _enum(EditIssueSeverity, self.severity)
        self.category = _enum(EditFailureCategory, self.category)

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class EditPlan:
    intent_id: str
    strategy: EditStrategyKind | str
    operations: list[EditOperation]
    rationale: list[str] = field(default_factory=list)
    scope: EditScope = field(default_factory=EditScope)
    id: str = field(default_factory=lambda: f"editplan_{uuid4().hex[:12]}")
    created_at: str = field(default_factory=lambda: _now())
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.strategy = _enum(EditStrategyKind, self.strategy)
        self.operations = [
            operation if isinstance(operation, EditOperation) else EditOperation(**dict(operation))
            for operation in self.operations
        ]
        if not isinstance(self.scope, EditScope):
            self.scope = EditScope(**dict(self.scope))

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class PatchCandidate:
    plan_id: str
    strategy: EditStrategyKind | str
    operations: list[Any]
    touched_paths: list[str]
    id: str = field(default_factory=lambda: f"patch_{uuid4().hex[:12]}")
    normalized_from: list[str] = field(default_factory=list)
    digest: str = ""
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self.strategy = _enum(EditStrategyKind, self.strategy)
        self.touched_paths = sorted(set(self.touched_paths))
        if not self.digest:
            payload = {
                "strategy": self.strategy.value,
                "paths": self.touched_paths,
                "operations": self.normalized_from,
            }
            self.digest = _digest(payload)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "plan_id": self.plan_id,
            "strategy": self.strategy.value,
            "touched_paths": self.touched_paths,
            "operation_count": len(self.operations),
            "normalized_from": self.normalized_from,
            "digest": self.digest,
            "metadata": _to_plain(self.metadata),
        }


@dataclass
class PatchValidationResult:
    ok: bool
    issues: list[EditIssue] = field(default_factory=list)
    requires_review: bool = False
    changed_files: list[str] = field(default_factory=list)
    diff_summary: list[dict[str, Any]] = field(default_factory=list)
    code_impact: dict[str, Any] | None = None
    test_impact: dict[str, Any] | None = None
    changeset_id: str | None = None
    failure_category: EditFailureCategory | str = EditFailureCategory.NONE
    changeset: Any | None = field(default=None, repr=False, compare=False)

    def __post_init__(self) -> None:
        self.issues = [
            issue if isinstance(issue, EditIssue) else EditIssue(**dict(issue))
            for issue in self.issues
        ]
        self.failure_category = _enum(EditFailureCategory, self.failure_category)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "requires_review": self.requires_review,
            "issues": [issue.to_dict() for issue in self.issues],
            "changed_files": self.changed_files,
            "diff_summary": self.diff_summary,
            "code_impact": self.code_impact,
            "test_impact": self.test_impact,
            "changeset_id": self.changeset_id,
            "failure_category": self.failure_category.value,
        }


@dataclass
class EditRepairAttempt:
    attempt: int
    category: EditFailureCategory | str
    action: str
    status: str
    message: str = ""
    candidate_id: str | None = None
    issues: list[EditIssue] = field(default_factory=list)
    created_at: str = field(default_factory=lambda: _now())

    def __post_init__(self) -> None:
        self.category = _enum(EditFailureCategory, self.category)
        self.issues = [
            issue if isinstance(issue, EditIssue) else EditIssue(**dict(issue))
            for issue in self.issues
        ]

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass
class EditResult:
    ok: bool
    status: str
    intent_id: str
    plan: EditPlan | None = None
    candidate: PatchCandidate | None = None
    validation: PatchValidationResult | None = None
    mutation_result: Any | None = None
    repair_attempts: list[EditRepairAttempt] = field(default_factory=list)
    changed_files: list[str] = field(default_factory=list)
    changeset_id: str | None = None
    transaction_id: str | None = None
    verification_plan: dict[str, Any] | None = None
    code_impact: dict[str, Any] | None = None
    test_impact: dict[str, Any] | None = None
    review_report: dict[str, Any] | None = None
    error_code: str | None = None
    message: str = ""
    id: str = field(default_factory=lambda: f"editresult_{uuid4().hex[:12]}")
    created_at: str = field(default_factory=lambda: _now())

    def to_dict(self) -> dict[str, Any]:
        mutation_observation = None
        mutation_status = None
        mutation_error = None
        if self.mutation_result is not None:
            mutation_status = getattr(self.mutation_result, "status", None)
            mutation_error = getattr(self.mutation_result, "error_code", None)
            mutation_observation = getattr(self.mutation_result, "observation", None)
        return {
            "edit_result_id": self.id,
            "ok": self.ok,
            "status": self.status,
            "intent_id": self.intent_id,
            "edit_plan_id": self.plan.id if self.plan else None,
            "strategy": self.plan.strategy.value if self.plan else None,
            "patch_candidate_id": self.candidate.id if self.candidate else None,
            "patch_digest": self.candidate.digest if self.candidate else None,
            "validation": self.validation.to_dict() if self.validation else None,
            "repair_attempts": [attempt.to_dict() for attempt in self.repair_attempts],
            "changed_files": self.changed_files,
            "changeset_id": self.changeset_id,
            "transaction_id": self.transaction_id,
            "verification_plan": self.verification_plan,
            "code_impact": self.code_impact,
            "test_impact": self.test_impact,
            "review_report": self.review_report,
            "mutation_status": mutation_status,
            "mutation_error_code": mutation_error,
            "mutation_observation": mutation_observation,
            "error_code": self.error_code,
            "message": self.message,
            "created_at": self.created_at,
        }


def _digest(value: Any) -> str:
    payload = json.dumps(_to_plain(value), ensure_ascii=False, sort_keys=True, default=str)
    return stable_hash_text(payload)


_enum = coerce_enum
_to_plain = to_plain_data
_now = utc_iso_timestamp
