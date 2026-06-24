from pathlib import Path

from singularity.code_index import (
    ContextCandidate,
    ProjectIndex,
    ProjectIndexConfig,
    ProjectIndexStore,
    WorkspaceScanner,
)
from singularity.code_index.models import Evidence, FreshnessStatus, SymbolKind, SymbolRecord


def test_store_upsert_query_stale_and_delete(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "app.py").write_text("def main(): pass\n", encoding="utf-8")
    record = WorkspaceScanner(tmp_path).scan()[0]
    store = ProjectIndexStore(tmp_path / ".singularity" / "index.sqlite")
    store.upsert_files([record])
    store.upsert_symbols(
        [
            SymbolRecord(
                path="src/app.py",
                name="main",
                qualified_name="app.main",
                kind=SymbolKind.FUNCTION,
                line_start=1,
                evidence=[Evidence(source="test", path="src/app.py")],
            )
        ]
    )

    assert store.files_by_path(["src/app.py"])["src/app.py"].sha256 == record.sha256
    assert store.query_symbols("main")[0].qualified_name == "app.main"

    store.mark_stale(["src/app.py"], FreshnessStatus.STALE_CONTENT)
    assert store.files_by_path(["src/app.py"])["src/app.py"].freshness == FreshnessStatus.STALE_CONTENT

    store.delete_by_path("src/app.py")
    assert store.files_by_path(["src/app.py"]) == {}


def test_project_index_incremental_query_and_impact_use_structured_facts(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "tests").mkdir()
    (tmp_path / "src" / "service.py").write_text(
        "def calculate():\n    return 1\n",
        encoding="utf-8",
    )
    (tmp_path / "src" / "api.py").write_text(
        "from src.service import calculate\n\ndef handler():\n    return calculate()\n",
        encoding="utf-8",
    )
    (tmp_path / "tests" / "test_service.py").write_text(
        "from src.service import calculate\n\ndef test_calculate():\n    assert calculate() == 1\n",
        encoding="utf-8",
    )

    component = ProjectIndex(tmp_path)
    summary = component.build_full_index(reason="test")
    relevant = component.find_relevant_files("change calculate service")
    impact = component.analyze_impact(["src/service.py"])
    test_impact = component.get_test_impact(["src/service.py"])
    context = component.get_context_candidates("change calculate service", budget_tokens=500)

    assert summary.file_count >= 3
    assert relevant[0].path == "src/service.py"
    assert "src/api.py" in impact.reverse_dependencies
    assert "tests/test_service.py" in test_impact.likely_tests
    assert all(isinstance(item, ContextCandidate) for item in context)

    (tmp_path / "src" / "service.py").write_text(
        "def calculate():\n    return 2\n",
        encoding="utf-8",
    )
    result = component.update_after_changeset({"changed_files": ["src/service.py"]}, reason="test")

    assert "src/service.py" in result.rebuilt_files
    assert result.summary["file_count"] >= 3


def test_disabled_project_index_bootstrap_has_no_store_side_effect(tmp_path: Path) -> None:
    component = ProjectIndex(tmp_path, config=ProjectIndexConfig(enabled=False))

    summary = component.bootstrap(reason="test")
    observation = component.observation_for_goal("inspect")
    health = component.health_check()
    impact = component.analyze_impact(["src/service.py"])
    test_impact = component.get_test_impact(["src/service.py"])
    update = component.update_after_changeset({"changed_files": ["src/service.py"]}, reason="test")

    assert summary.limitations == ["project_index_disabled"]
    assert observation["warnings"] == ["project_index_disabled"]
    assert health["ok"] is True
    assert health["summary"]["file_count"] == 0
    assert impact.risk_reasons == ["project_index_disabled"]
    assert test_impact.require_full_test is True
    assert update.changed_files == ["src/service.py"]
    assert not (tmp_path / ".singularity" / "index.sqlite").exists()


def test_full_index_rebuild_failure_preserves_previous_index(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "service.py").write_text("def calculate():\n    return 1\n", encoding="utf-8")
    component = ProjectIndex(tmp_path)
    initial = component.build_full_index(reason="initial")

    def fail_extract(_files):
        raise RuntimeError("extract failed")

    component._extract_facts = fail_extract  # type: ignore[method-assign]
    try:
        component.build_full_index(reason="failing")
    except RuntimeError:
        pass
    else:
        raise AssertionError("index rebuild failure was swallowed")

    assert component.store.load_summary().file_count == initial.file_count
    assert component.store.files_by_path(["src/service.py"])
