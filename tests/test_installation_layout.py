from __future__ import annotations

import json
import shutil
import zipfile
from pathlib import Path

import pytest
from typer.testing import CliRunner

from singularity.cli import app
from singularity.release.init import initialize_user_data
from singularity.release.migrations import Migration, apply_migrations, load_manifest
from singularity.release.models import atomic_write_json, read_json
from singularity.release.paths import UserDataMode, resolve_user_data_paths


runner = CliRunner()


def test_user_data_paths_honor_singularity_home(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "home"))

    paths = resolve_user_data_paths()

    assert paths.mode == UserDataMode.USER
    assert paths.root == (tmp_path / "home").resolve()
    assert paths.config_dir == paths.root / "config"
    assert paths.state_dir == paths.root / "state"
    assert paths.traces_dir == paths.root / "traces"


def test_system_init_is_idempotent_and_does_not_overwrite_config(
    monkeypatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    first = runner.invoke(app, ["system", "init", "--json"])
    paths = resolve_user_data_paths()
    paths.config_file.write_text('{"schema_version": 1, "custom": true}\n', encoding="utf-8")

    second = runner.invoke(app, ["system", "init", "--json"])

    assert first.exit_code == 0
    assert second.exit_code == 0
    assert json.loads(second.output)["config_written"] is False
    assert json.loads(paths.config_file.read_text(encoding="utf-8"))["custom"] is True
    assert paths.manifest_file.exists()


def test_doctor_json_reports_component_health_without_real_home(
    monkeypatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0

    result = runner.invoke(app, ["doctor", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["schema_version"] == "diagnostic-result/v1"
    assert payload["ok"] is True
    finding_ids = {finding["check_id"] for finding in payload["findings"]}
    assert {"environment.python", "config.file", "schema.migrations"} <= finding_ids
    check_names = {check["name"] for check in payload["checks"]}
    assert {"python_version", "config_schema", "migrations", "optional_dependencies"} <= check_names


def test_version_json_uses_project_version_in_source_checkout(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))

    result = runner.invoke(app, ["version", "--json"])

    assert result.exit_code == 0
    assert json.loads(result.output)["version"] == "0.1.0"


def test_migration_failure_rolls_back_config_and_manifest(tmp_path: Path) -> None:
    paths = resolve_user_data_paths(home=tmp_path / "component")
    initialize_user_data(paths)
    original_config = {"schema_version": 1, "component": {"mode": "user"}, "policy": {}, "sandbox": {}, "model": {}, "provider": {}}
    atomic_write_json(paths.config_file, original_config)
    manifest = load_manifest(paths).to_dict()
    manifest["last_migration"] = "000"
    atomic_write_json(paths.manifest_file, manifest)

    def boom(user_data_paths):
        atomic_write_json(user_data_paths.config_file, {"schema_version": 1, "broken": True})
        raise RuntimeError("migration failed")

    with pytest.raises(RuntimeError):
        apply_migrations(paths, migrations=[Migration("999", "boom", boom)])

    assert read_json(paths.config_file) == original_config
    assert load_manifest(paths).last_migration == "000"


def test_system_repair_restores_missing_defaults(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0
    paths = resolve_user_data_paths()
    paths.config_file.unlink()
    shutil.rmtree(paths.tmp_dir)

    result = runner.invoke(app, ["system", "repair", "--json"])

    assert result.exit_code == 0
    assert paths.config_file.exists()
    assert paths.tmp_dir.exists()
    payload = json.loads(result.output)
    assert payload["after"]["ok"] is True


def test_uninstall_dry_run_preserves_user_data_by_default(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0
    paths = resolve_user_data_paths()
    (paths.memory_dir / "note.txt").write_text("keep", encoding="utf-8")
    (paths.traces_dir / "run.jsonl").write_text("keep", encoding="utf-8")

    result = runner.invoke(app, ["system", "uninstall", "--dry-run", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert str(paths.config_dir) in payload["delete"]
    assert str(paths.memory_dir) in payload["preserve"]
    assert str(paths.traces_dir) in payload["preserve"]
    assert paths.config_dir.exists()
    assert paths.memory_dir.exists()


def test_uninstall_blocks_unowned_user_data_home(monkeypatch, tmp_path: Path) -> None:
    home = tmp_path / "not-owned"
    (home / "config").mkdir(parents=True)
    (home / "state").mkdir()
    monkeypatch.setenv("SINGULARITY_HOME", str(home))

    result = runner.invoke(
        app,
        ["system", "uninstall", "--purge-user-data", "--dry-run", "--json"],
    )

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["blocked"] is True
    assert payload["delete"] == []


def test_system_export_writes_relative_user_data_archive(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0
    paths = resolve_user_data_paths()
    (paths.memory_dir / "note.txt").write_text("memory", encoding="utf-8")
    output = tmp_path / "export.zip"

    result = runner.invoke(app, ["system", "export", "--output", str(output), "--json"])

    assert result.exit_code == 0
    with zipfile.ZipFile(output) as archive:
        names = set(archive.namelist())
    assert "manifest.json" in names
    assert "memory/note.txt" in names


def test_system_export_does_not_include_output_zip_itself(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setenv("SINGULARITY_HOME", str(tmp_path / "component"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0
    paths = resolve_user_data_paths()
    output = paths.backups_dir / "self.zip"

    result = runner.invoke(app, ["system", "export", "--output", str(output), "--json"])

    assert result.exit_code == 0
    with zipfile.ZipFile(output) as archive:
        names = set(archive.namelist())
    assert "backups/self.zip" not in names


def test_pyproject_console_script_targets_cli_main() -> None:
    import tomllib

    payload = tomllib.loads(Path("pyproject.toml").read_text(encoding="utf-8"))

    assert payload["project"]["scripts"] == {
        "singularity-agent": "singularity.cli:main",
        "sg": "singularity.cli:main",
    }
    assert "platformdirs>=4.2" in payload["project"]["dependencies"]
    assert "eval" in payload["project"]["optional-dependencies"]
    assert "test" in payload["dependency-groups"]
    assert "editables>=0.5" in payload["build-system"]["requires"]
