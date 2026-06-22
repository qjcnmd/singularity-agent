from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from singularity.cli import app
from singularity.git_runtime import GitRuntime


runner = CliRunner()


def _require_git() -> None:
    result = subprocess.run(
        ["git", "--version"],
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        pytest.skip("git executable is not available")


def _init_repo(path: Path) -> None:
    _require_git()
    subprocess.run(["git", "init"], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "singularity@example.invalid"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Singularity Tests"],
        cwd=path,
        check=True,
        capture_output=True,
    )


def test_git_runtime_reports_status_diff_and_local_commit(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    runtime = GitRuntime(repo)

    (repo / "example.txt").write_text("one\n", encoding="utf-8")
    status = runtime.status()

    assert status.available is True
    assert "?? example.txt" in status.entries

    created = runtime.commit("add example", paths=["example.txt"])
    assert created.ok is True
    assert created.commit
    assert created.files == ["example.txt"]

    (repo / "example.txt").write_text("one\ntwo\n", encoding="utf-8")
    diff = runtime.diff_stat()

    assert diff.files == 1
    assert diff.insertions >= 1
    assert diff.paths == ["example.txt"]


def test_git_cli_status_and_diff_json(monkeypatch, tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    (repo / "example.txt").write_text("one\n", encoding="utf-8")
    subprocess.run(["git", "add", "example.txt"], cwd=repo, check=True, capture_output=True)
    subprocess.run(["git", "commit", "-m", "initial"], cwd=repo, check=True, capture_output=True)
    (repo / "example.txt").write_text("one\ntwo\n", encoding="utf-8")
    monkeypatch.chdir(repo)

    status = runner.invoke(app, ["git", "status", "--json"])
    diff = runner.invoke(app, ["git", "diff", "--json"])

    assert status.exit_code == 0, status.output
    assert diff.exit_code == 0, diff.output
    assert json.loads(status.output)["available"] is True
    assert json.loads(diff.output)["paths"] == ["example.txt"]
