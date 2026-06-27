from __future__ import annotations

import os
import tempfile
from pathlib import Path

import pytest


_PYTEST_TEMP_ROOT = Path(__file__).resolve().parents[1] / "work" / "pytest-tmp-root"
_PYTEST_TEMP_ROOT.mkdir(parents=True, exist_ok=True)

for _name in ("TMPDIR", "TEMP", "TMP"):
    os.environ[_name] = str(_PYTEST_TEMP_ROOT)
tempfile.tempdir = str(_PYTEST_TEMP_ROOT)


# ---------------------------------------------------------------------------
# Smoke suite: curated tests that cover core paths in <30 seconds.
# Two mechanisms: _SMOKE_TEST_IDS for specific tests, _SMOKE_FILE_PREFIXES
# for files where every test is included.
# ---------------------------------------------------------------------------
_SMOKE_TEST_IDS: set[str] = {
    # CLI startup
    "tests/test_cli.py::test_cli_runs_through_kernel_bootstrap",
    # Context assembly
    "tests/test_context.py::test_context_manager_initializes_system_and_user_messages",
    # Planner decision
    "tests/test_planner.py::test_start_task_builds_state_plan_and_persists",
    # Approval gate
    "tests/test_approval_gate.py::test_interactive_approve_once_generates_single_use_grant",
    # Verification path
    "tests/test_verification_runner.py::test_verification_runner_executes_checks_through_command_executor_and_records_trace",
}

# Files where every test is a smoke test (fast, core-path coverage).
_SMOKE_FILE_STEMS: set[str] = {
    "test_policy_engine",      # 12 tests — all policy decision outcomes
    "test_tool_contract",      # 5 tests — tool contract validation
    "test_trace",              # 1 test — trace writer foundation
    "test_tool_protocol_result",  # 2 tests — result builder redaction
}

# ---------------------------------------------------------------------------
# Flaky tests: known intermittent failures.
# Default run still includes them; see docs/testing.md for handling policy.
# ---------------------------------------------------------------------------
_FLAKY_TEST_IDS: set[str] = {
    "tests/test_cli.py::test_cli_eval_task_validate_and_list_filter_tags",
    "tests/test_cli.py::test_cli_eval_private_uses_private_benchmark_adapter",
    "tests/test_tool_executor_secret_safety.py::test_list_files_hides_sensitive_paths_by_default",
    "tests/test_observability_integration.py::test_tool_executor_dispatch_emits_structured_trace",
}

# ---------------------------------------------------------------------------
# Slow tests: truly slow (>5s) due to concurrency or multi-turn simulation.
# Based on measured --durations data, not filename heuristics.
# ---------------------------------------------------------------------------
_SLOW_TEST_IDS: set[str] = {
    # Agent loop simulations (>5s)
    "tests/test_execution_primitives_phase1b.py::test_deterministic_quicksort_tasks_complete_with_write_file_and_apply_patch",
    "tests/test_cli.py::test_cli_eval_targeted_replay_writes_repair_replay_artifacts",
    "tests/test_agent_task_outcome.py::test_verification_failure_replans_instead_of_completing",
    "tests/test_agent_task_outcome.py::test_low_confidence_analysis_blocks_repair_contract",
    "tests/test_agent_task_outcome.py::test_repeated_failure_fingerprint_budget_blocks_after_second_failed_verification",
    "tests/test_agent_task_outcome.py::test_unrepairable_verification_failure_blocks_with_user_input_required",
    "tests/test_agent_task_outcome.py::test_premature_final_then_quicksort_smoke_completes",
    "tests/test_agent_task_outcome.py::test_tool_failure_then_verification_failure_replans_repairs_and_finalizes",
    # Concurrency tests (>5s)
    "tests/test_span_manager.py::test_span_manager_concurrent_spans_do_not_interfere",
}

# ---------------------------------------------------------------------------
# External-dependency tests: require docker/git/network but are fast when
# available. Separated from "slow" to avoid misleading duration expectations.
# ---------------------------------------------------------------------------
_EXTERNAL_FILE_KEYWORDS: tuple[str, ...] = (
    "sandbox_backend_docker",
    "sandbox_backend_windows",
)

# Explicit test name keywords that indicate external dependency.
_EXTERNAL_TEST_KEYWORDS: tuple[str, ...] = (
    "real_docker",
    "backend_windows",
)


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    """Apply markers to tests based on file path, test name, and curated lists.

    Priority rules:
    1. Explicit ``@pytest.mark.X`` decorators on the test function/class take
       absolute priority — auto-classification never overwrites them.
    2. ``evaluation`` directory is a hard convention: everything under
       ``tests/evaluation/`` gets the ``evaluation`` marker regardless.
    3. Curated lists (smoke, slow, flaky) are applied unconditionally.
    4. Auto-classification (security, regression, integration, unit) is the
       fallback for tests with no explicit markers.
    """
    _EVALUATION_DIR = Path(__file__).parent / "evaluation"

    for item in items:
        explicit = _explicit_marker_names(item)

        # --- evaluation: hard directory convention ---
        try:
            Path(item.fspath).relative_to(_EVALUATION_DIR)
            _add_marker(item, "evaluation")
            continue  # evaluation tests don't get further classification
        except ValueError:
            pass

        # --- smoke: curated list (additive, never conflicts) ---
        file_stem = Path(item.fspath).stem
        if item.nodeid in _SMOKE_TEST_IDS or file_stem in _SMOKE_FILE_STEMS:
            _add_marker(item, "smoke")

        # --- flaky: curated list (additive) ---
        if item.nodeid in _FLAKY_TEST_IDS:
            _add_marker(item, "flaky")

        # --- slow: curated list based on measured durations ---
        if item.nodeid in _SLOW_TEST_IDS:
            _add_marker(item, "slow")

        # --- external: docker/git/network dependency ---
        file_str = str(item.fspath)
        name = item.name
        if any(kw in file_str for kw in _EXTERNAL_FILE_KEYWORDS) or \
           any(kw in name for kw in _EXTERNAL_TEST_KEYWORDS):
            _add_marker(item, "external")

        # --- If the test has an explicit functional marker, stop here ---
        # provider_eval is always respected; other explicit markers mean
        # the developer already chose a category.
        if explicit - {"smoke", "flaky", "slow", "external"}:
            continue

        # --- Auto-classify by file path / test name ---
        # security: trust boundary tests
        if any(kw in name for kw in ("security", "redaction", "secret", "injection")):
            _add_marker(item, "security")
            continue
        if any(kw in file_str for kw in ("security", "redaction", "secret", "injection")):
            _add_marker(item, "security")
            continue

        # regression: production baseline / docs / identity
        if any(kw in file_str for kw in (
            "production", "docs_consistency", "runtime_docs",
            "runtime_sqlite", "singularity_identity",
        )):
            _add_marker(item, "regression")
            continue

        # integration: multi-component subdirectories
        integration_dirs = (
            "code_index", "diagnostics", "edit", "interaction",
            "memory", "plugins", "review",
        )
        if any(d in file_str for d in integration_dirs):
            _add_marker(item, "integration")
            continue
        if "integration" in file_str:
            _add_marker(item, "integration")
            continue

        # unit: everything else
        _add_marker(item, "unit")


def _explicit_marker_names(item: pytest.Item) -> set[str]:
    """Return marker names that were explicitly applied via decorators."""
    return {m.name for m in item.iter_markers()}


def _add_marker(item: pytest.Item, name: str) -> None:
    """Add a marker only if not already present."""
    if name not in {m.name for m in item.iter_markers()}:
        item.add_marker(getattr(pytest.mark, name))


@pytest.fixture(autouse=True)
def _isolate_policy_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Redirect default policy paths to a per-test tmp directory.

    Trust boundary: default approval-grant and audit-log paths were moved to
    ``~/.singularity/policy/``. Tests must not write to the real home
    directory, so each test gets an isolated policy home under ``tmp_path``
    via the ``SINGULARITY_POLICY_HOME`` environment variable. This only
    affects the policy modules and does not patch ``Path.home()`` globally.
    """

    monkeypatch.setenv("SINGULARITY_POLICY_HOME", str(tmp_path))
