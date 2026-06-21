from __future__ import annotations

import json

from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.release.paths import resolve_runtime_paths


runner = CliRunner()


def test_doctor_cli_json_uses_diagnostic_result_schema(monkeypatch, tmp_path):
    monkeypatch.setenv("MINIHARNESS_HOME", str(tmp_path / "runtime"))
    assert runner.invoke(app, ["system", "init"]).exit_code == 0

    result = runner.invoke(app, ["doctor", "--json", "--check", "environment.python"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["schema_version"] == "diagnostic-result/v1"
    assert payload["filters"]["check_id"] == "environment.python"
    assert payload["findings"][0]["severity"] == "info"


def test_repair_cli_defaults_to_dry_run(monkeypatch, tmp_path):
    monkeypatch.setenv("MINIHARNESS_HOME", str(tmp_path / "runtime"))

    result = runner.invoke(app, ["repair", "--json"])
    paths = resolve_runtime_paths()

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["schema_version"] == "repair-result/v1"
    assert payload["ok"] is True
    assert payload["repair"]["applied"] is False
    assert payload["repair"]["actions"]
    assert payload["after"] is None
    assert not paths.config_file.exists()


def test_repair_cli_apply_runs_then_reports_remaining_errors(monkeypatch, tmp_path):
    monkeypatch.setenv("MINIHARNESS_HOME", str(tmp_path / "runtime"))

    result = runner.invoke(app, ["repair", "--apply", "--check", "filesystem.runtime_dirs", "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["schema_version"] == "repair-result/v1"
    assert payload["ok"] is True
    assert payload["repair"]["applied"] is True
    assert payload["after"]["schema_version"] == "diagnostic-result/v1"
    assert payload["after"]["ok"] is True


def test_repair_cli_apply_returns_nonzero_when_action_fails(monkeypatch, tmp_path):
    monkeypatch.setenv("MINIHARNESS_HOME", str(tmp_path / "runtime"))
    monkeypatch.chdir(tmp_path)
    assert runner.invoke(app, ["system", "init"]).exit_code == 0
    broken_entries = tmp_path / ".miniharness" / "memory" / "auto" / "entries.jsonl"
    broken_entries.parent.mkdir(parents=True)
    broken_entries.write_text("{broken", encoding="utf-8")

    result = runner.invoke(app, ["repair", "--apply", "--check", "schema.memory_index", "--json"])

    assert result.exit_code == 1
    payload = json.loads(result.output)
    assert payload["schema_version"] == "repair-result/v1"
    assert payload["ok"] is False
    assert payload["repair"]["actions"][0]["status"] == "failed"
    assert broken_entries.read_text(encoding="utf-8") == "{broken"
