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


@pytest.fixture(autouse=True)
def _isolate_policy_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Redirect default policy paths to a per-test tmp directory.

    P0-1 moved default approval-grant and audit-log paths to
    ``~/.singularity/policy/``. Tests must not write to the real home
    directory, so each test gets an isolated policy home under ``tmp_path``
    via the ``SINGULARITY_POLICY_HOME`` environment variable. This only
    affects the policy modules and does not patch ``Path.home()`` globally.
    """

    monkeypatch.setenv("SINGULARITY_POLICY_HOME", str(tmp_path))
