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
# Auto-apply pytest markers based on file path conventions.
# ---------------------------------------------------------------------------
#
# Marker priority (first match wins):
#   1. Explicit @pytest.mark.X on the test function/class
#   2. File-path convention:
#      - tests/evaluation/  -> evaluation
#      - *security*         -> security
#      - *production*       -> regression
#      - *docs_consistency*, *runtime_docs*, *runtime_sqlite*, *singularity_identity* -> regression
#      - tests/code_index/  -> integration
#      - tests/diagnostics/ -> integration
#      - tests/edit/        -> integration
#      - tests/interaction/ -> integration
#      - tests/memory/      -> integration
#      - tests/plugins/     -> integration
#      - tests/review/      -> integration
#      - tests/             -> unit (default)
#   3. Slow heuristic: tests with "docker" or "backend_windows" in name -> slow

_EVALUATION_DIR = Path(__file__).parent / "evaluation"


def pytest_collection_modifyitems(config: pytest.Config, items: list[pytest.Item]) -> None:
    """Apply markers to tests that don't already have them."""
    for item in items:
        item_path = Path(item.fspath)
        test_name = item.name

        # --- evaluation: tests/evaluation/ ---
        try:
            item_path.relative_to(_EVALUATION_DIR)
            if "evaluation" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.evaluation)
            continue  # evaluation tests don't get further classification
        except ValueError:
            pass

        # --- provider_eval: already marked by decorator ---
        if "provider_eval" in {m.name for m in item.iter_markers()}:
            continue

        # --- security: filename contains "security" or "redaction" or "secret" or "injection" ---
        if any(kw in test_name for kw in ("security", "redaction", "secret", "injection")):
            if "security" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.security)
            continue

        # --- security: file-level ---
        if any(kw in str(item_path) for kw in ("security", "redaction", "secret", "injection")):
            if "security" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.security)
            continue

        # --- regression: production baseline / docs / identity ---
        if any(kw in str(item_path) for kw in ("production", "docs_consistency", "runtime_docs", "runtime_sqlite", "singularity_identity")):
            if "regression" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.regression)
            continue

        # --- slow: docker / windows backends ---
        if any(kw in test_name for kw in ("docker", "backend_windows")):
            if "slow" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.slow)
            # These are also integration tests
            if "integration" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.integration)
            continue

        # --- integration: multi-component subdirectories ---
        integration_dirs = ("code_index", "diagnostics", "edit", "interaction", "memory", "plugins", "review")
        if any(d in str(item_path) for d in integration_dirs):
            if "integration" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.integration)
            continue

        # --- integration: files with "integration" in name ---
        if "integration" in str(item_path):
            if "integration" not in {m.name for m in item.iter_markers()}:
                item.add_marker(pytest.mark.integration)
            continue

        # --- unit: everything else ---
        if "unit" not in {m.name for m in item.iter_markers()}:
            item.add_marker(pytest.mark.unit)


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
