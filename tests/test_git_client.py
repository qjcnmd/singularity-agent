from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from singularity.oracle.cli import app
from singularity.git_client import GitClient

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


def test_git_client_reports_status_diff_and_local_commit(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    component = GitClient(repo)

    (repo / "example.txt").write_text("one\n", encoding="utf-8")
    status = component.status()

    assert status.available is True
    assert "?? example.txt" in status.entries

    created = component.commit("add example", paths=["example.txt"])
    assert created.ok is True
    assert created.commit
    assert created.files == ["example.txt"]

    (repo / "example.txt").write_text("one\ntwo\n", encoding="utf-8")
    diff = component.diff_stat()

    assert diff.files == 1
    assert diff.insertions >= 1
    assert diff.paths == ["example.txt"]


def test_git_client_commit_requires_explicit_paths(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    component = GitClient(repo)
    (repo / "tracked.txt").write_text("tracked\n", encoding="utf-8")
    (repo / "untracked.txt").write_text("untracked\n", encoding="utf-8")

    result = component.commit("unsafe default")

    assert result.ok is False
    assert result.exit_code == 2
    assert "Explicit paths are required" in result.stderr
    status = subprocess.run(
        ["git", "status", "--short"],
        cwd=repo,
        text=True,
        capture_output=True,
        check=True,
    )
    assert "?? tracked.txt" in status.stdout
    assert "?? untracked.txt" in status.stdout


def test_git_client_allow_empty_does_not_stage_untracked_paths(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    component = GitClient(repo)
    (repo / "untracked.txt").write_text("untracked\n", encoding="utf-8")

    result = component.commit("empty commit", allow_empty=True)

    assert result.ok is True
    assert result.commit
    status = subprocess.run(
        ["git", "status", "--short"],
        cwd=repo,
        text=True,
        capture_output=True,
        check=True,
    )
    assert "?? untracked.txt" in status.stdout


def test_git_client_rejects_paths_outside_workspace(tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_repo(repo)
    component = GitClient(repo)
    outside = tmp_path / "outside.txt"
    outside.write_text("outside\n", encoding="utf-8")

    with pytest.raises(ValueError, match="outside workspace"):
        component.commit("outside", paths=[str(outside)])


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


def test_git_cli_accepts_explicit_project_root(monkeypatch, tmp_path: Path) -> None:
    repo = tmp_path / "repo"
    cwd = tmp_path / "cwd"
    repo.mkdir()
    cwd.mkdir()
    _init_repo(repo)
    (repo / "example.txt").write_text("one\n", encoding="utf-8")
    monkeypatch.chdir(cwd)

    status = runner.invoke(app, ["git", "status", "--project-root", str(repo), "--json"])

    assert status.exit_code == 0, status.output
    payload = json.loads(status.output)
    assert payload["available"] is True
    assert Path(payload["workspace_root"]) == repo.resolve()
