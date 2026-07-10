from __future__ import annotations

import os
import tempfile
import warnings
from pathlib import Path

import pytest

_PYTEST_TEMP_ROOT = Path(__file__).resolve().parents[1] / "work" / "pytest-tmp-root"
_PYTEST_TEMP_ROOT.mkdir(parents=True, exist_ok=True)

for _name in ("TMPDIR", "TEMP", "TMP"):
    os.environ[_name] = str(_PYTEST_TEMP_ROOT)
tempfile.tempdir = str(_PYTEST_TEMP_ROOT)


# ---------------------------------------------------------------------------
# Known markers (must match pyproject.toml [tool.pytest.ini_options].markers).
# Used for self-check validation.
# ---------------------------------------------------------------------------
_KNOWN_MARKERS: set[str] = {
    "smoke",
    "unit",
    "integration",
    "regression",
    "security",
    "evaluation",
    "provider_eval",
    "slow",
    "external",
    "flaky",
}

_FUNCTIONAL_MARKERS: set[str] = {
    "unit",
    "integration",
    "regression",
    "security",
    "evaluation",
    "provider_eval",
}


# ---------------------------------------------------------------------------
# Smoke suite: curated tests that cover core paths in <30 seconds.
# Two mechanisms: _SMOKE_TEST_IDS for specific tests, _SMOKE_FILE_STEMS
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
    # Test impact analysis
    "tests/test_test_impact.py::TestFallbackTests::test_test_impact_fallback_basic_mapping",
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
# Slow tests: truly slow (>3s) due to concurrency or multi-turn simulation.
# Based on measured --durations data, not filename heuristics.
# ---------------------------------------------------------------------------
_SLOW_TEST_IDS: set[str] = {
    # Agent loop simulations (>5s)
    "tests/test_execution_primitives_phase1b.py::test_deterministic_quicksort_tasks_complete_with_write_file_and_apply_patch",
    "tests/test_cli.py::test_cli_eval_targeted_replay_writes_repair_replay_artifacts",
    # Agent loop simulations (3-5s, multi-turn with verification)
    "tests/test_agent_task_outcome.py::test_verification_failure_replans_instead_of_completing",
    "tests/test_agent_task_outcome.py::test_low_confidence_analysis_blocks_repair_contract",
    "tests/test_agent_task_outcome.py::test_repeated_failure_fingerprint_budget_blocks_after_second_failed_verification",
    "tests/test_agent_task_outcome.py::test_unrepairable_verification_failure_blocks_with_user_input_required",
    "tests/test_agent_task_outcome.py::test_premature_final_then_quicksort_smoke_completes",
    "tests/test_agent_task_outcome.py::test_tool_failure_then_verification_failure_replans_repairs_and_finalizes",
    # Newly identified from --durations=50 analysis (>3s agent simulations)
    "tests/test_agent_task_outcome.py::test_analyzer_model_failure_blocks_with_user_input_required",
    "tests/test_agent_task_outcome.py::test_invalid_analyzer_json_blocks_without_repairing",
    "tests/test_agent_task_outcome.py::test_unauthorized_affected_files_block_repair_contract",
    "tests/test_agent_task_outcome.py::test_ready_verification_finalizes_without_extra_model_turn",
    "tests/test_agent_task_outcome.py::test_policy_denial_blocks_without_bypassing_policy",
    # Concurrency tests (>5s)
    "tests/test_span_manager.py::test_span_manager_concurrent_spans_do_not_interfere",
    # AgentLoop wiring tests (3-4s)
    "tests/test_agent.py::test_agent_injects_workspace_state_observation_after_tool_call",
    "tests/test_agent.py::test_agent_runs_complete_tool_call_loop",
    # Planner concurrency (3s)
    "tests/test_planner.py::test_planner_store_concurrent_append_events_do_not_interleave",
    # Workspace lock cross-process (2.2s, uses multiprocessing)
    "tests/test_workspace_lock.py::test_workspace_lock_allows_only_one_writer_across_processes",
}

# ---------------------------------------------------------------------------
# External-dependency tests: require OS sandbox/git/network but are fast when
# available. Separated from "slow" to avoid misleading duration expectations.
# ---------------------------------------------------------------------------
_EXTERNAL_FILE_KEYWORDS: tuple[str, ...] = ()

# Files whose tests all require real git subprocess calls.
_EXTERNAL_FILE_STEMS: set[str] = {
    "test_git_client",
    "test_targeted_failure_replay",
    "test_singularity_identity",
    "test_runtime_sqlite_artifacts",
}

# Explicit test name keywords that indicate external dependency.
_EXTERNAL_TEST_KEYWORDS: tuple[str, ...] = ()

# ---------------------------------------------------------------------------
# Integration-dense files: tests that wire real subsystems together but are
# not caught by the integration-directory / "integration" keyword heuristics.
# These are tests doing agent simulation, component.run(), subprocess,
# multiprocessing, threading, or real git/network wiring.
# ---------------------------------------------------------------------------
_INTEGRATION_FILE_STEMS: set[str] = {
    # Agent simulation (full AgentLoop wiring with MockProvider)
    "test_agent",
    "test_agent_task_outcome",
    "test_agent_graph",
    "test_execution_primitives_phase1b",
    "test_cancellation",
    "test_approval_gate",
    "test_repair_contract_verification",
    "test_kernel_finalization",
    # Multi-component wiring (component.run / component.run_plan)
    "test_command_executor",
    "test_tool_executor",
    "test_tool_executor_cache",
    "test_tool_executor_backend",
    "test_verification_runner",
    "test_planner",
    "test_semantic_planner",
    "test_semantic_planner_capability",
    "test_final_reviewer",
    "test_task_controller",
    "test_lifecycle_manager",
    "test_shutdown_manager",
    # Subprocess / multiprocessing / threading concurrency
    "test_workspace_lock",
    "test_span_manager",
    "test_test_impact",
    # Real external dependencies (git / OS sandbox / network)
    "test_git_client",
    "test_sandbox_manager",
    "test_sandbox_backend_windows",
    "test_sandbox_environment",
    "test_sandbox_filesystem",
    "test_sandbox_models",
    "test_sandbox_artifacts",
    # Remote approval / workspace mutation
    "test_remote_approval",
    "test_workspace_mutation",
    "test_workspace_state_manager",
}


# ---------------------------------------------------------------------------
# Curated list self-check: validate that all curated nodeids exist in the
# collected test session and that marker assignments are consistent.
# ---------------------------------------------------------------------------
def _validate_curated_lists(
    config: pytest.Config,
    items: list[pytest.Item],
) -> None:
    """Validate curated test ID lists against the actual test collection.

    Emits warnings for stale entries.  Never raises, so stale curated lists
    don't block test execution — but the warnings are visible and can be
    promoted to errors with ``-W error::pytest.PytestWarning``.

    Nodeid-based checks are skipped when the collection is obviously a
    subset (e.g. running a single test file), because curated IDs naturally
    won't be present.
    """
    collected_nodeids = {item.nodeid for item in items}
    collected_stems = {Path(item.fspath).stem for item in items}

    is_full_collection = _is_full_suite_collection(config, items)

    if is_full_collection:
        for label, id_set in [
            ("smoke", _SMOKE_TEST_IDS),
            ("flaky", _FLAKY_TEST_IDS),
            ("slow", _SLOW_TEST_IDS),
        ]:
            stale = id_set - collected_nodeids
            if stale:
                warnings.warn(
                    pytest.PytestWarning(
                        f"[test-infra] Stale {label} test IDs not found in collection: "
                        + ", ".join(sorted(stale))
                    ),
                    stacklevel=2,
                )

        # Check file-stem-based lists
        for label, stem_set in [
            ("smoke files", _SMOKE_FILE_STEMS),
            ("external files", _EXTERNAL_FILE_STEMS),
        ]:
            stale = stem_set - collected_stems
            if stale:
                warnings.warn(
                    pytest.PytestWarning(
                        f"[test-infra] Stale {label} stems not found: "
                        + ", ".join(sorted(stale))
                    ),
                    stacklevel=2,
                )

    # Overlap checks are always safe (no dependency on collection size)
    overlap = _SMOKE_TEST_IDS & _SLOW_TEST_IDS
    if overlap:
        warnings.warn(
            pytest.PytestWarning(
                "[test-infra] Tests in both smoke and slow: "
                + ", ".join(sorted(overlap))
            ),
            stacklevel=2,
        )

    overlap_stems = _SMOKE_FILE_STEMS & _EXTERNAL_FILE_STEMS
    if overlap_stems:
        warnings.warn(
            pytest.PytestWarning(
                "[test-infra] Files in both smoke and external: "
                + ", ".join(sorted(overlap_stems))
            ),
            stacklevel=2,
        )

    # --- Integration file stems check (full-collection only) ---
    if is_full_collection:
        stale_int = _INTEGRATION_FILE_STEMS - collected_stems
        if stale_int:
            warnings.warn(
                pytest.PytestWarning(
                    "[test-infra] Stale integration file stems not found: "
                    + ", ".join(sorted(stale_int))
                ),
                stacklevel=2,
            )

    # --- Slow/external tests must have non-unit functional classification ---
    for item in items:
        item_markers = {m.name for m in item.iter_markers()}
        is_slow_or_external = ("slow" in item_markers or "external" in item_markers)
        if not is_slow_or_external:
            continue
        functional = item_markers & _FUNCTIONAL_MARKERS
        if "unit" in functional:
            warnings.warn(
                pytest.PytestWarning(
                    f"[test-infra] Slow/external test classified as unit: "
                    f"{item.nodeid}"
                ),
                stacklevel=2,
            )
        elif not functional:
            warnings.warn(
                pytest.PytestWarning(
                    f"[test-infra] Slow/external test has no functional "
                    f"classification: {item.nodeid}"
                ),
                stacklevel=2,
            )

    # --- Smoke must not overlap with provider_eval ---
    smoke_provider_overlap: list[str] = []
    for item in items:
        item_markers = {m.name for m in item.iter_markers()}
        if "smoke" in item_markers and "provider_eval" in item_markers:
            smoke_provider_overlap.append(item.nodeid)
    if smoke_provider_overlap:
        warnings.warn(
            pytest.PytestWarning(
                "[test-infra] Tests in both smoke and provider_eval: "
                + ", ".join(sorted(smoke_provider_overlap))
            ),
            stacklevel=2,
        )


def _module_imports_integration_indicators(item: pytest.Item) -> bool:
    """Check whether the test module imports known integration-signaling libraries.

    Integration indicators are imports that reveal the test is doing real
    subprocess calls, multiprocessing, threading concurrency, or agent
    simulation — not pure function/class unit tests.  This catches new test
    files that aren't yet in ``_INTEGRATION_FILE_STEMS``.

    Only acts on ``pytest.Function`` items (the leaf test nodes).  Silently
    returns ``False`` for other item types so they fall through to the normal
    auto-classification chain.
    """
    import sys

    if not isinstance(item, pytest.Function):
        return False

    module_name = item.module.__name__ if hasattr(item, "module") else None
    if module_name is None or module_name not in sys.modules:
        return False

    module = sys.modules[module_name]
    module_names = set(dir(module))

    # Libraries whose mere import is a strong signal of integration work.
    _INTEGRATION_LIBS = {
        "subprocess",           # real process spawning
        "multiprocessing",      # cross-process testing
        "threading",            # concurrent thread testing
    }

    # Symbols from test helpers that indicate agent simulation.
    _AGENT_SIMULATION_SYMBOLS = {
        "make_agent_session",
        "MockProvider",
        "MockToolRunner",
        "AgentLoop",
    }

    # Symbols that indicate real Docker / git / network wiring.
    _EXTERNAL_WIRING_SYMBOLS = {
        "DockerSandboxBackend",
        "GitClient",
        "subprocess",  # already in libs but also here for clarity
    }

    if module_names & _INTEGRATION_LIBS:
        return True
    if module_names & _AGENT_SIMULATION_SYMBOLS:
        return True
    return bool(module_names & _EXTERNAL_WIRING_SYMBOLS)


def _is_full_suite_collection(config: pytest.Config, items: list[pytest.Item]) -> bool:
    """Return true only for broad suite collections, not targeted subsets."""
    if len(items) < 100:
        return False

    args = tuple(str(arg) for arg in getattr(config, "args", ()) or ())
    if not args:
        return True

    root = Path(getattr(config, "rootpath", Path.cwd())).resolve()
    tests_root = Path(__file__).resolve().parent
    return all(
        not _is_targeted_collection_arg(arg, root=root, tests_root=tests_root)
        for arg in args
    )


def _is_targeted_collection_arg(arg: str, *, root: Path, tests_root: Path) -> bool:
    target = arg.split("::", 1)[0]
    if not target or target.startswith("-"):
        return False

    path = Path(target)
    if not path.is_absolute():
        path = root / path

    try:
        resolved = path.resolve()
    except OSError:
        return False

    if resolved in {root, tests_root.resolve()}:
        return False

    try:
        resolved.relative_to(tests_root.resolve())
    except ValueError:
        return False
    return True


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

        # --- external: platform/git/network dependency ---
        file_str = str(item.fspath)
        name = item.name
        if (
            any(kw in file_str for kw in _EXTERNAL_FILE_KEYWORDS)
            or file_stem in _EXTERNAL_FILE_STEMS
            or any(kw in name for kw in _EXTERNAL_TEST_KEYWORDS)
        ):
            _add_marker(item, "external")

        # --- If the test has an explicit functional marker, stop here ---
        # pytest control markers such as parametrize do not choose a
        # functional category, so slow/external tests still need fallback
        # classification.
        if explicit & _FUNCTIONAL_MARKERS:
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

        # integration-dense files: curated stems for files that wire real
        # subsystems but are not caught by directory/keyword heuristics above.
        if file_stem in _INTEGRATION_FILE_STEMS:
            _add_marker(item, "integration")
            continue

        # unit: everything else — but slow/external curated tests MUST NOT
        # be classified as unit.  Check the item's current markers: if it
        # already has slow or external, assign integration instead of unit.
        # Also check the test module's imports for integration indicators
        # (subprocess, multiprocessing, threading, agent_loop_helpers) so
        # that new test files doing real integration work are not silently
        # misclassified as unit.
        current_markers = _explicit_marker_names(item)
        if "slow" in current_markers or "external" in current_markers or _module_imports_integration_indicators(item):
            _add_marker(item, "integration")
        else:
            _add_marker(item, "unit")

    # --- Self-check: validate curated lists against actual collection ---
    _validate_curated_lists(config, items)


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
