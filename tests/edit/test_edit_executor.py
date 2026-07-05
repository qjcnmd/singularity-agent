from __future__ import annotations

import json
from pathlib import Path

from singularity.edit import EditExecutor, EditIntent, EditOperation, EditScope, EditStrategyKind
from singularity.edit.models import (
    EDIT_SCOPE_DEFAULT_MAX_CANDIDATES,
    EDIT_SCOPE_DEFAULT_MAX_FILES,
    EDIT_SCOPE_DEFAULT_MAX_REPAIR_ATTEMPTS,
    EDIT_SCOPE_DEFAULT_REWRITE_MAX_CHANGED_LINES,
    EDIT_SCOPE_DEFAULT_TARGETED_MAX_CHANGED_LINES,
    EDIT_SCOPE_DEFAULT_TARGETED_MAX_FILE_CHANGE_RATIO,
)
from singularity.review import (
    ReviewDecision,
    ReviewDecisionAction,
    ReviewReport,
    ReviewStage,
    ReviewTarget,
)
from singularity.tools.edit import (
    EDIT_SCOPE_INPUT_MAX_CANDIDATES_LIMIT,
    EDIT_SCOPE_INPUT_MAX_FILES_LIMIT,
    EDIT_SCOPE_INPUT_MAX_REPAIR_ATTEMPTS_LIMIT,
    EditScopeInput,
)
from singularity.workspace import WorkspaceMutationManager


class _Impact:
    def __init__(self, **payload):
        self.payload = payload

    def to_dict(self):
        return dict(self.payload)


class _Index:
    def __init__(self, *, high: bool = False) -> None:
        self.high = high

    def analyze_impact(self, paths):
        paths = list(paths)
        return _Impact(
            requested_paths=paths,
            direct_files=paths,
            reverse_dependencies=[],
            affected_symbols=[],
            affected_entrypoints=["app:main"] if self.high else [],
            affected_tests=[],
            config_impact=False,
            generated_or_vendor_impact=False,
            broad_impact=self.high,
            risk_level="high" if self.high else "low",
            risk_reasons=["broad impact"] if self.high else [],
            recommended_validation=[],
        )

    def get_test_impact(self, changed_files):
        files = list(changed_files)
        return _Impact(
            changed_files=files,
            likely_tests=[f"tests/test_{Path(files[0]).stem}.py"] if files else [],
            commands=["pytest"] if files else [],
            require_full_test=False,
            confidence_note="fake",
        )


class _Verification:
    def __init__(self) -> None:
        self.plans = []

    def plan_verification(self, **kwargs):
        plan = _Impact(id="verify_1", verification_plan_id="verify_1", **kwargs)
        self.plans.append(plan)
        return plan


class _ReviewProbe:
    def __init__(self, *, pre_action: str = "accept", post_action: str = "accept") -> None:
        self.pre_action = ReviewDecisionAction(pre_action)
        self.post_action = ReviewDecisionAction(post_action)
        self.pre_calls = []
        self.post_calls = []

    def pre_edit_review(self, **kwargs):
        self.pre_calls.append(kwargs)
        return ReviewReport(
            target=ReviewTarget(stage=ReviewStage.PRE_EDIT),
            input_summary="pre",
            decision=ReviewDecision(action=self.pre_action, reasons=["pre"]),
        )

    def post_patch_review(self, **kwargs):
        self.post_calls.append(kwargs)
        return ReviewReport(
            target=ReviewTarget(stage=ReviewStage.POST_PATCH),
            input_summary="post",
            decision=ReviewDecision(action=self.post_action, reasons=["post"]),
        )


def _component(tmp_path: Path, *, index=None, verification=None, review=None) -> EditExecutor:
    mutation = WorkspaceMutationManager(tmp_path, project_index=index)
    return EditExecutor(
        tmp_path,
        mutation_manager=mutation,
        project_index=index,
        verification_runner=verification,
        review_pipeline=review,
    )


def test_edit_scope_defaults_are_shared_with_input_model_constants() -> None:
    scope = EditScope()
    scope_input = EditScopeInput()
    model_fields = EditScopeInput.model_fields

    assert scope.max_files == EDIT_SCOPE_DEFAULT_MAX_FILES
    assert scope.targeted_max_changed_lines == EDIT_SCOPE_DEFAULT_TARGETED_MAX_CHANGED_LINES
    assert scope.targeted_max_file_change_ratio == EDIT_SCOPE_DEFAULT_TARGETED_MAX_FILE_CHANGE_RATIO
    assert scope.rewrite_max_changed_lines == EDIT_SCOPE_DEFAULT_REWRITE_MAX_CHANGED_LINES
    assert scope.max_repair_attempts == EDIT_SCOPE_DEFAULT_MAX_REPAIR_ATTEMPTS
    assert scope.max_candidates == EDIT_SCOPE_DEFAULT_MAX_CANDIDATES

    assert scope_input.max_files == EDIT_SCOPE_DEFAULT_MAX_FILES
    assert scope_input.targeted_max_changed_lines == EDIT_SCOPE_DEFAULT_TARGETED_MAX_CHANGED_LINES
    assert scope_input.targeted_max_file_change_ratio == EDIT_SCOPE_DEFAULT_TARGETED_MAX_FILE_CHANGE_RATIO
    assert scope_input.rewrite_max_changed_lines == EDIT_SCOPE_DEFAULT_REWRITE_MAX_CHANGED_LINES
    assert scope_input.max_repair_attempts == EDIT_SCOPE_DEFAULT_MAX_REPAIR_ATTEMPTS
    assert scope_input.max_candidates == EDIT_SCOPE_DEFAULT_MAX_CANDIDATES

    assert model_fields["max_files"].default == EDIT_SCOPE_DEFAULT_MAX_FILES
    assert model_fields["max_files"].metadata[-1].le == EDIT_SCOPE_INPUT_MAX_FILES_LIMIT
    assert model_fields["max_repair_attempts"].default == EDIT_SCOPE_DEFAULT_MAX_REPAIR_ATTEMPTS
    assert model_fields["max_repair_attempts"].metadata[-1].le == EDIT_SCOPE_INPUT_MAX_REPAIR_ATTEMPTS_LIMIT
    assert model_fields["max_candidates"].default == EDIT_SCOPE_DEFAULT_MAX_CANDIDATES
    assert model_fields["max_candidates"].metadata[-1].le == EDIT_SCOPE_INPUT_MAX_CANDIDATES_LIMIT


def test_targeted_patch_unique_context_replacement(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("".join([*["# pad\n" for _ in range(20)], "print('old')\n"]), encoding="utf-8")
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="rename output",
            operations=[
                EditOperation(
                    kind="replace_text",
                    path="app.py",
                    old_text="old",
                    new_text="new",
                )
            ],
        )
    )

    assert result.ok is True
    assert result.plan.strategy == EditStrategyKind.TARGETED_PATCH
    assert "print('new')" in source.read_text(encoding="utf-8")
    assert result.transaction_id


def test_targeted_patch_marker_insert_and_range_replace(tmp_path: Path) -> None:
    source = tmp_path / "notes.txt"
    source.write_text(
        "".join([*["# pad\n" for _ in range(20)], "start\n# marker\nold line\nend\n"]),
        encoding="utf-8",
    )
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="insert and replace",
            operations=[
                EditOperation(
                    kind="insert_after",
                    path="notes.txt",
                    marker="# marker\n",
                    text="inserted\n",
                ),
                EditOperation(
                    kind="replace_range",
                    path="notes.txt",
                    start_line=24,
                    end_line=24,
                    new_text="new line",
                ),
            ],
        )
    )

    assert result.ok is True
    assert source.read_text(encoding="utf-8").endswith("start\n# marker\ninserted\nnew line\nend\n")


def test_full_file_rewrite_existing_and_create_file(tmp_path: Path) -> None:
    (tmp_path / "existing.txt").write_text("old\n", encoding="utf-8")
    component = _component(tmp_path)

    rewrite = component.apply_intent(
        EditIntent(
            summary="rewrite file",
            operations=[
                EditOperation(kind="rewrite_file", path="existing.txt", content="new\n")
            ],
        )
    )
    create = component.apply_intent(
        EditIntent(
            summary="create file",
            operations=[
                EditOperation(kind="create_file", path="created.txt", content="hello\n")
            ],
        )
    )

    assert rewrite.ok is True
    assert rewrite.plan.strategy == EditStrategyKind.FULL_FILE_REWRITE
    assert create.ok is True
    assert (tmp_path / "existing.txt").read_text(encoding="utf-8") == "new\n"
    assert (tmp_path / "created.txt").read_text(encoding="utf-8") == "hello\n"


def test_full_file_rewrite_large_diff_requires_review(tmp_path: Path) -> None:
    old = "".join(f"old {index}\n" for index in range(520))
    new = "".join(f"new {index}\n" for index in range(520))
    (tmp_path / "big.txt").write_text(old, encoding="utf-8")
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="big rewrite",
            operations=[EditOperation(kind="rewrite_file", path="big.txt", content=new)],
        )
    )

    assert result.ok is False
    assert result.status == "requires_review"
    assert (tmp_path / "big.txt").read_text(encoding="utf-8") == old


def test_structured_json_update_and_python_symbol_replace(tmp_path: Path) -> None:
    (tmp_path / "config.json").write_text('{"tool": {"enabled": false}}\n', encoding="utf-8")
    source = tmp_path / "app.py"
    source.write_text("def greet():\n    return 'old'\n", encoding="utf-8")
    component = _component(tmp_path)

    json_result = component.apply_intent(
        EditIntent(
            summary="update json",
            operations=[
                EditOperation(
                    kind="update_json",
                    path="config.json",
                    updates={"tool": {"enabled": True}},
                )
            ],
        )
    )
    py_result = component.apply_intent(
        EditIntent(
            summary="replace function",
            operations=[
                EditOperation(
                    kind="replace_symbol",
                    path="app.py",
                    symbol_name="greet",
                    new_text="def greet():\n    return 'new'\n",
                )
            ],
        )
    )

    assert json_result.ok is True
    assert json.loads((tmp_path / "config.json").read_text(encoding="utf-8"))["tool"]["enabled"] is True
    assert py_result.ok is True
    assert "return 'new'" in source.read_text(encoding="utf-8")


def test_structured_python_syntax_risk_is_rejected(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("def greet():\n    return 'old'\n", encoding="utf-8")
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="replace with invalid python",
            operations=[
                EditOperation(
                    kind="replace_symbol",
                    path="app.py",
                    symbol_name="greet",
                    new_text="def greet(:\n    return 'bad'\n",
                )
            ],
        )
    )

    assert result.ok is False
    assert result.validation.failure_category.value == "syntax_risk"
    assert "return 'old'" in source.read_text(encoding="utf-8")


def test_validation_blocks_forbidden_path_hash_stale_context_conflict_and_high_impact(tmp_path: Path) -> None:
    (tmp_path / "dup.txt").write_text("same\nsame\n", encoding="utf-8")
    (tmp_path / "stale.txt").write_text("one\n", encoding="utf-8")
    forbidden = _component(tmp_path).apply_intent(
        EditIntent(
            summary="secret",
            operations=[EditOperation(kind="create_file", path=".env", content="TOKEN=x\n")],
        )
    )
    stale = _component(tmp_path).preview_intent(
        EditIntent(
            summary="stale",
            operations=[EditOperation(kind="replace_range", path="stale.txt", start_line=1, end_line=1, new_text="two")],
            scope=EditScope(expected_hashes={"stale.txt": "wrong"}),
        )
    )
    conflict = _component(tmp_path).preview_intent(
        EditIntent(
            summary="ambiguous",
            operations=[EditOperation(kind="replace_text", path="dup.txt", old_text="same", new_text="other")],
        )
    )
    high_impact = _component(tmp_path, index=_Index(high=True)).preview_intent(
        EditIntent(
            summary="impact",
            operations=[EditOperation(kind="replace_range", path="dup.txt", start_line=1, end_line=1, new_text="new")],
        )
    )

    assert forbidden.ok is False
    assert not (tmp_path / ".env").exists()
    assert stale.validation.failure_category.value == "freshness"
    assert conflict.validation.failure_category.value == "context_mismatch"
    assert high_impact.status == "requires_review"


def test_over_modification_targeted_patch_requires_review(tmp_path: Path) -> None:
    original = "".join(f"line {index}\n" for index in range(20))
    replacement = "".join(f"new {index}\n" for index in range(20))
    (tmp_path / "small.txt").write_text(original, encoding="utf-8")
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="too much",
            operations=[
                EditOperation(
                    kind="replace_text",
                    path="small.txt",
                    old_text=original,
                    new_text=replacement,
                )
            ],
        )
    )

    assert result.ok is False
    assert result.status == "requires_review"
    assert (tmp_path / "small.txt").read_text(encoding="utf-8") == original


def test_repair_refreshes_stale_hash_and_context_fallback_succeeds(tmp_path: Path) -> None:
    (tmp_path / "stale.txt").write_text(
        "".join([*["pad\n" for _ in range(20)], "current\n"]),
        encoding="utf-8",
    )
    (tmp_path / "fallback.txt").write_text(
        "".join([*["pad\n" for _ in range(20)], "line one\nline two\n"]),
        encoding="utf-8",
    )
    component = _component(tmp_path)

    refreshed = component.apply_intent(
        EditIntent(
            summary="refresh stale hash",
            operations=[
                EditOperation(
                    kind="replace_range",
                    path="stale.txt",
                    start_line=21,
                    end_line=21,
                    new_text="updated",
                    expected_sha256="wrong",
                )
            ],
            scope=EditScope(expected_hashes={"stale.txt": "wrong"}, targeted_max_file_change_ratio=1.0),
        )
    )
    fallback = component.apply_intent(
        EditIntent(
            summary="fallback to range",
            operations=[
                EditOperation(
                    kind="replace_text",
                    path="fallback.txt",
                    old_text="missing",
                    new_text="line TWO",
                    start_line=22,
                    end_line=22,
                )
            ],
        )
    )

    assert refreshed.ok is True
    assert refreshed.repair_attempts[0].category.value == "freshness"
    assert (tmp_path / "stale.txt").read_text(encoding="utf-8").endswith("updated\n")
    assert fallback.ok is True
    assert fallback.repair_attempts[0].action == "safe_strategy_fallback"
    assert (tmp_path / "fallback.txt").read_text(encoding="utf-8").endswith("line one\nline TWO\n")


def test_repair_failure_does_not_retry_non_recoverable_categories(tmp_path: Path) -> None:
    component = _component(tmp_path)

    result = component.apply_intent(
        EditIntent(
            summary="forbidden no retry",
            operations=[EditOperation(kind="create_file", path=".env", content="TOKEN=x\n")],
            scope=EditScope(max_repair_attempts=2),
        )
    )

    assert result.ok is False
    assert result.repair_attempts == []
    assert result.error_code == "policy_denied"
    assert result.validation.failure_category.value == "policy_denied"


def test_apply_records_post_apply_impact_and_verification_plan(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("".join([*["# pad\n" for _ in range(20)], "print('old')\n"]), encoding="utf-8")
    verification = _Verification()
    component = _component(tmp_path, index=_Index(), verification=verification)

    result = component.apply_intent(
        EditIntent(
            summary="impact verify",
            operations=[EditOperation(kind="replace_text", path="app.py", old_text="old", new_text="new")],
        )
    )

    assert result.ok is True
    assert result.code_impact["direct_files"] == ["app.py"]
    assert result.test_impact["likely_tests"] == ["tests/test_app.py"]
    assert result.verification_plan["verification_plan_id"] == "verify_1"
    assert verification.plans


def test_pre_edit_review_blocks_apply_before_preview_or_mutation(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("".join([*["# pad\n" for _ in range(20)], "print('old')\n"]), encoding="utf-8")
    review = _ReviewProbe(pre_action="replan")
    component = _component(tmp_path, review=review)

    result = component.apply_intent(
        EditIntent(
            summary="rename output",
            operations=[EditOperation(kind="replace_text", path="app.py", old_text="old", new_text="new")],
        )
    )

    assert result.ok is False
    assert result.status == "review_replan"
    assert result.review_report["decision"]["action"] == "replan"
    assert review.pre_calls
    assert review.post_calls == []
    assert "print('old')" in source.read_text(encoding="utf-8")


def test_post_patch_review_report_is_attached_after_apply(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("".join([*["# pad\n" for _ in range(20)], "print('old')\n"]), encoding="utf-8")
    review = _ReviewProbe()
    component = _component(tmp_path, review=review)

    result = component.apply_intent(
        EditIntent(
            summary="rename output",
            operations=[EditOperation(kind="replace_text", path="app.py", old_text="old", new_text="new")],
        )
    )

    assert result.ok is True
    assert result.review_report["target"]["stage"] == "post_patch"
    assert review.pre_calls
    assert review.post_calls


def test_preview_does_not_trigger_post_patch_review(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("".join([*["# pad\n" for _ in range(20)], "print('old')\n"]), encoding="utf-8")
    review = _ReviewProbe()
    component = _component(tmp_path, review=review)

    result = component.preview_intent(
        EditIntent(
            summary="rename output",
            operations=[EditOperation(kind="replace_text", path="app.py", old_text="old", new_text="new")],
        )
    )

    assert result.ok is True
    assert result.status == "preview"
    assert review.pre_calls
    assert review.post_calls == []
    assert "print('old')" in source.read_text(encoding="utf-8")
