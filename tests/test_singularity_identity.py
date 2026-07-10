from __future__ import annotations

import importlib
import os
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"


def test_python_cli_module_is_not_public_runtime_entrypoint() -> None:
    module = importlib.import_module("singularity")
    assert module.__name__ == "singularity"

    result = subprocess.run(
        [sys.executable, "-m", "singularity.cli", "--help"],
        cwd=ROOT,
        env={**os.environ, "PYTHONPATH": str(SRC)},
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "run" not in combined
    assert "eval" not in combined
    assert "sandbox" not in combined


def test_pyproject_exposes_only_singularity_console_scripts() -> None:
    data = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))

    assert data["project"]["name"] == "singularity-agent"
    assert "scripts" not in data["project"]


def test_tracked_files_do_not_use_retired_project_identity() -> None:
    retired_terms = [bytes.fromhex(value).decode("ascii") for value in [
        "4d696e694861726e657373",
        "4d696e696861726e657373",
        "6d696e696861726e657373",
        "4d494e494841524e455353",
        "2e6d696e696861726e657373",
        "4d696e694167656e74",
        "6d696e696167656e74",
        "782d6d696e696861726e657373",
        "5f6d696e696861726e657373",
    ]]
    files = subprocess.run(
        ["git", "ls-files", "README.md", "pyproject.toml", "src", "tests", "docs"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=True,
    ).stdout.splitlines()

    offenders: list[str] = []
    for name in files:
        path = ROOT / name
        if not path.exists():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for term in retired_terms:
            if term in text:
                offenders.append(f"{name}: {term}")

    assert offenders == []
