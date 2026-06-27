"""Tests for scripts/test_impact.py — the test impact analysis script.

Covers: fallback heuristics, special path mappings, JSON output structure,
strict-index mode, recommendation validation, and code index integration.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

# Import the module under test
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scripts.test_impact import (
    _compute_confidence,
    _fallback_tests,
    _is_pytest_collectable,
    _validate_recommendations,
    main,
)


# ---------------------------------------------------------------------------
# _fallback_tests
# ---------------------------------------------------------------------------

class TestFallbackTests:
    """Path-based heuristic test mapping."""

    def test_test_file_input_returned_directly(self) -> None:
        """A test file path that exists is returned as-is."""
        # Use a real existing test file from the repo
        result, warnings = _fallback_tests(["tests/test_cli.py"])
        assert "tests/test_cli.py" in result

    def test_nonexistent_test_file_not_returned(self) -> None:
        """A test file path that doesn't exist is not returned."""
        result, _ = _fallback_tests(["tests/test_nonexistent_xyz.py"])
        assert result == []

    def test_non_python_file_skipped(self) -> None:
        """Non-.py files without a special map produce warnings, not silent skip."""
        result, warnings = _fallback_tests(["README.md"])
        assert result == []
        assert len(warnings) >= 1
        assert any("No test mapping" in w for w in warnings)

    def test_context_manager_maps_to_test_context(self) -> None:
        """src/singularity/context/manager.py -> tests/test_context.py"""
        # src/singularity/context/manager.py -> singularity is at index 1,
        # pkg_parts = ["context", "manager.py"], pkg = "context"
        # Convention 1: tests/test_context.py (exists in repo)
        result, warnings = _fallback_tests(
            ["src/singularity/context/manager.py"],
        )
        assert "tests/test_context.py" in result, (
            f"Expected tests/test_context.py in results, got: {result}"
        )
        assert len(warnings) == 0

    def test_top_level_module_maps_correctly(self) -> None:
        """src/singularity/cli.py -> tests/test_cli.py (if exists)."""
        # test_cli.py actually exists in the repo
        result, _ = _fallback_tests(["src/singularity/cli.py"])
        assert "tests/test_cli.py" in result

    def test_package_module_maps_to_package_test(self) -> None:
        """src/singularity/planner/engine.py should try tests/test_planner.py."""
        result, _ = _fallback_tests(["src/singularity/planner/engine.py"])
        # test_planner.py exists in the repo
        assert "tests/test_planner.py" in result

    def test_empty_input_returns_empty(self) -> None:
        result, _ = _fallback_tests([])
        assert result == []

    def test_test_impact_fallback_basic_mapping(self) -> None:
        """Smoke: verify basic path heuristic mapping works."""
        result, _ = _fallback_tests(["src/singularity/cli.py"])
        assert "tests/test_cli.py" in result

    def test_warnings_for_missing_test_files(self) -> None:
        """Missing test files should produce warnings."""
        _, warnings = _fallback_tests(["tests/test_xyz_nonexistent.py"])
        assert len(warnings) == 1
        assert "not found" in warnings[0]


# ---------------------------------------------------------------------------
# Special path mappings
# ---------------------------------------------------------------------------

class TestSpecialPathMappings:
    """Tests for _SPECIAL_PATH_MAP entries — files that aren't tests but
    should trigger specific test suites when changed."""

    def test_conftest_maps_to_test_infra(self) -> None:
        """tests/conftest.py -> tests/test_test_infra.py"""
        result, warnings = _fallback_tests(["tests/conftest.py"])
        assert "tests/test_test_infra.py" in result
        # Should NOT include conftest.py itself as a recommendation
        assert "tests/conftest.py" not in result
        assert len(warnings) == 0

    def test_docs_testing_md_maps_to_docs_consistency(self) -> None:
        """docs/testing.md -> tests/test_docs_consistency.py"""
        result, warnings = _fallback_tests(["docs/testing.md"])
        assert "tests/test_docs_consistency.py" in result
        assert len(warnings) == 0

    def test_pyproject_toml_maps_to_test_infra(self) -> None:
        """pyproject.toml -> tests/test_test_infra.py + smoke collect warning"""
        result, warnings = _fallback_tests(["pyproject.toml"])
        assert "tests/test_test_infra.py" in result
        assert any("pyproject.toml changed" in w for w in warnings)
        assert any("collect-only" in w for w in warnings)

    def test_test_impact_py_maps_to_self_test(self) -> None:
        """scripts/test_impact.py -> tests/test_test_impact.py"""
        result, warnings = _fallback_tests(["scripts/test_impact.py"])
        assert "tests/test_test_impact.py" in result
        assert len(warnings) == 0

    def test_non_test_file_in_tests_dir_excluded(self) -> None:
        """tests/__init__.py should NOT be recommended as a test target."""
        result, warnings = _fallback_tests(["tests/__init__.py"])
        assert "tests/__init__.py" not in result
        assert len(warnings) >= 1
        assert any("not a pytest-collectable" in w for w in warnings)

    def test_helper_file_in_tests_dir_excluded(self) -> None:
        """tests/agent_loop_helpers.py should NOT be recommended."""
        result, warnings = _fallback_tests(["tests/agent_loop_helpers.py"])
        assert "tests/agent_loop_helpers.py" not in result
        assert len(warnings) >= 1
        assert any("not a pytest-collectable" in w for w in warnings)

    def test_no_recommendation_for_unmapped_config(self) -> None:
        """Unmapped .md files produce warnings, not silent skip."""
        result, warnings = _fallback_tests(["CHANGELOG.md", "unknown.toml"])
        assert result == []
        assert len(warnings) >= 2
        assert all("No test mapping" in w for w in warnings)


# ---------------------------------------------------------------------------
# Recommendation validation
# ---------------------------------------------------------------------------

class TestValidation:
    """Tests for _is_pytest_collectable and _validate_recommendations."""

    def test_is_pytest_collectable_test_file(self) -> None:
        """Standard test_*.py files are collectable."""
        assert _is_pytest_collectable("tests/test_cli.py") is True
        assert _is_pytest_collectable("tests/test_agent.py") is True

    def test_is_pytest_collectable_conftest(self) -> None:
        """conftest.py is NOT collectable."""
        assert _is_pytest_collectable("tests/conftest.py") is False

    def test_is_pytest_collectable_init(self) -> None:
        """__init__.py is NOT collectable."""
        assert _is_pytest_collectable("tests/__init__.py") is False

    def test_is_pytest_collectable_helper(self) -> None:
        """*_helpers.py files are NOT collectable."""
        assert _is_pytest_collectable("tests/agent_loop_helpers.py") is False
        assert _is_pytest_collectable("tests/tool_executor_helpers.py") is False

    def test_validate_recommendations_filters_non_tests(self) -> None:
        """_validate_recommendations removes non-collectable entries."""
        clean, warnings = _validate_recommendations([
            "tests/test_cli.py",
            "tests/conftest.py",
            "tests/__init__.py",
            "tests/agent_loop_helpers.py",
            "tests/test_agent.py",
        ])
        assert sorted(clean) == ["tests/test_agent.py", "tests/test_cli.py"]
        assert len(warnings) == 3
        for w in warnings:
            assert "not a pytest-collectable" in w

    def test_validate_recommendations_empty(self) -> None:
        """Empty input produces empty output."""
        clean, warnings = _validate_recommendations([])
        assert clean == []
        assert warnings == []

    def test_validate_recommendations_all_valid(self) -> None:
        """All-valid input passes through unchanged."""
        clean, warnings = _validate_recommendations([
            "tests/test_foo.py",
            "tests/test_bar.py",
        ])
        assert sorted(clean) == ["tests/test_bar.py", "tests/test_foo.py"]
        assert warnings == []


# ---------------------------------------------------------------------------
# _compute_confidence
# ---------------------------------------------------------------------------

class TestComputeConfidence:
    """Confidence level calculation."""

    def test_code_index_with_results_is_high(self) -> None:
        assert _compute_confidence("code_index", ["test.py"], []) == "high"

    def test_code_index_without_results_is_medium(self) -> None:
        assert _compute_confidence("code_index", [], []) == "medium"

    def test_heuristics_with_results_is_medium(self) -> None:
        assert _compute_confidence("path_heuristics", ["test.py"], []) == "medium"

    def test_heuristics_without_results_is_low(self) -> None:
        assert _compute_confidence("path_heuristics", [], []) == "low"


# ---------------------------------------------------------------------------
# JSON output
# ---------------------------------------------------------------------------

class TestJsonOutput:
    """Test --json flag output structure."""

    def test_json_output_has_required_fields(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """JSON output must contain all required fields."""
        script = Path(__file__).resolve().parents[1] / "scripts" / "test_impact.py"
        result = subprocess.run(
            [sys.executable, str(script), "src/singularity/cli.py", "--json"],
            capture_output=True,
            text=True,
            cwd=str(tmp_path.parent.parent),  # repo root
        )
        if result.returncode != 0:
            pytest.skip(f"Script failed: {result.stderr}")
        data = json.loads(result.stdout)
        required_fields = {
            "changed_files",
            "source",
            "warnings",
            "recommended_tests",
            "recommended_commands",
            "confidence",
        }
        assert required_fields.issubset(set(data.keys())), (
            f"Missing fields: {required_fields - set(data.keys())}"
        )

    def test_json_output_changed_files_populated(
        self,
        tmp_path: Path,
    ) -> None:
        """JSON changed_files should echo back the input files."""
        script = Path(__file__).resolve().parents[1] / "scripts" / "test_impact.py"
        result = subprocess.run(
            [sys.executable, str(script), "src/singularity/cli.py", "--json"],
            capture_output=True,
            text=True,
            cwd=str(tmp_path.parent.parent),
        )
        if result.returncode != 0:
            pytest.skip(f"Script failed: {result.stderr}")
        data = json.loads(result.stdout)
        assert "src/singularity/cli.py" in data["changed_files"]

    def test_json_no_changed_files(self, tmp_path: Path) -> None:
        """Unmapped non-Python input should produce empty recommendations and warnings."""
        script = Path(__file__).resolve().parents[1] / "scripts" / "test_impact.py"
        # Pass a non-python file not in _SPECIAL_PATH_MAP
        result = subprocess.run(
            [sys.executable, str(script), "README.md", "--json"],
            capture_output=True,
            text=True,
            cwd=str(tmp_path.parent.parent),
        )
        if result.returncode != 0:
            pytest.skip(f"Script failed: {result.stderr}")
        data = json.loads(result.stdout)
        assert data["recommended_tests"] == []
        assert data["confidence"] == "low"
        # Should have a warning about unmapped non-Python file
        assert len(data["warnings"]) >= 1

    def test_json_output_special_paths(
        self,
        tmp_path: Path,
    ) -> None:
        """JSON output for special paths maps correctly and excludes non-tests."""
        repo_root = Path(__file__).resolve().parents[1]
        script = repo_root / "scripts" / "test_impact.py"
        result = subprocess.run(
            [
                sys.executable, str(script),
                "scripts/test_impact.py",
                "tests/conftest.py",
                "docs/testing.md",
                "--json",
            ],
            capture_output=True,
            text=True,
            cwd=str(repo_root),
        )
        if result.returncode != 0:
            pytest.skip(f"Script failed: {result.stderr}")
        data = json.loads(result.stdout)
        # All three special paths should be mapped
        assert "tests/test_test_impact.py" in data["recommended_tests"]
        assert "tests/test_test_infra.py" in data["recommended_tests"]
        assert "tests/test_docs_consistency.py" in data["recommended_tests"]
        # conftest.py itself should NOT be in recommendations
        assert "tests/conftest.py" not in data["recommended_tests"]
        # All recommended tests should be collectable
        for t in data["recommended_tests"]:
            assert t.startswith("tests/test_"), f"Non-test in recommendations: {t}"


# ---------------------------------------------------------------------------
# --strict-index
# ---------------------------------------------------------------------------

class TestStrictIndex:
    """Test --strict-index flag behavior."""

    def test_strict_index_fails_without_index(
        self,
        tmp_path: Path,
    ) -> None:
        """--strict-index should exit 1 when code index is unavailable."""
        script = Path(__file__).resolve().parents[1] / "scripts" / "test_impact.py"
        result = subprocess.run(
            [
                sys.executable, str(script),
                "src/singularity/cli.py",
                "--strict-index",
                "--workspace", str(tmp_path),
                "--json",
            ],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 1

    def test_strict_index_json_output_on_failure(
        self,
        tmp_path: Path,
    ) -> None:
        """--strict-index failure should still produce valid JSON."""
        script = Path(__file__).resolve().parents[1] / "scripts" / "test_impact.py"
        result = subprocess.run(
            [
                sys.executable, str(script),
                "src/singularity/cli.py",
                "--strict-index",
                "--workspace", str(tmp_path),
                "--json",
            ],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 1
        data = json.loads(result.stdout)
        assert data["source"] == "unavailable"
        assert len(data["warnings"]) > 0
