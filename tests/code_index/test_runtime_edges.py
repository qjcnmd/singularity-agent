from pathlib import Path

from miniharness.code_index import ProjectIndexRuntime
from miniharness.verification import CheckKind, VerificationRuntime
from miniharness.workspace import MutationRuntime, ReplaceText


def test_verification_runtime_uses_project_index_targeted_test_mapping(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "tests").mkdir()
    (tmp_path / "src" / "service.py").write_text("def calculate():\n    return 1\n", encoding="utf-8")
    (tmp_path / "tests" / "test_service.py").write_text("def test_calculate():\n    assert True\n", encoding="utf-8")
    (tmp_path / "pyproject.toml").write_text("[project]\nname = 'demo'\n", encoding="utf-8")

    index = ProjectIndexRuntime(tmp_path)
    index.build_full_index(reason="test")
    verification = VerificationRuntime(tmp_path, project_index_runtime=index)

    plan = verification.plan_verification(
        changed_files=["src/service.py"],
        task_intent="change calculate",
    )

    assert any(check.kind == CheckKind.UNIT_TEST and check.scope == "code_index_targeted_tests" for check in plan.required_checks)
    assert plan.impact_analysis.index_source == "ProjectIndexRuntime"
    assert any(item["test_path"] == "tests/test_service.py" for item in plan.impact_analysis.test_mappings)


def test_mutation_runtime_uses_project_index_for_entrypoint_risk_escalation(tmp_path: Path) -> None:
    (tmp_path / "app.py").write_text("def main():\n    return 1\n", encoding="utf-8")
    index = ProjectIndexRuntime(tmp_path)
    index.build_full_index(reason="test")
    mutation = MutationRuntime(tmp_path, project_index_runtime=index)

    result = mutation.apply_operations(
        [ReplaceText(path="app.py", old_text="return 1", new_text="return 2")],
        intent="change app entrypoint",
        created_by="test",
    )

    assert result.ok is False
    assert result.status == "requires_review"
    assert result.error_code == "approval_required"
    assert (tmp_path / "app.py").read_text(encoding="utf-8").endswith("return 1\n")
