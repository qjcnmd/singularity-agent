from __future__ import annotations

import json
import os

from singularity.diagnostics import DoctorEngine
from singularity.release.init import default_config, initialize_runtime
from singularity.release.models import atomic_write_json
from singularity.release.paths import resolve_runtime_paths


def test_filesystem_check_reports_missing_runtime_dirs_as_repairable(tmp_path):
    paths = resolve_runtime_paths(home=tmp_path / "runtime")

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="filesystem")

    finding = next(item for item in result.findings if item.check_id == "filesystem.runtime_dirs")
    assert finding.status == "failed"
    assert finding.auto_repairable is True
    assert str(paths.config_dir) in finding.details["missing"]


def test_filesystem_check_reports_unwritable_directory(monkeypatch, tmp_path):
    paths = resolve_runtime_paths(home=tmp_path / "runtime")
    initialize_runtime(paths)
    original_access = os.access

    def fake_access(path, mode):
        if str(path) == str(paths.logs_dir):
            return False
        return original_access(path, mode)

    monkeypatch.setattr(os, "access", fake_access)

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="filesystem.runtime_dirs")

    finding = result.findings[0]
    assert finding.status == "failed"
    assert str(paths.logs_dir) in finding.details["unwritable"]
    assert finding.auto_repairable is False


def test_config_check_reports_missing_fields_without_leaking_api_key(monkeypatch, tmp_path):
    monkeypatch.setenv("SINGULARITY_API_KEY", "sk-secret-value")
    paths = resolve_runtime_paths(home=tmp_path / "runtime")
    initialize_runtime(paths)
    atomic_write_json(paths.config_file, {"schema_version": 1, "runtime": {"mode": "user"}})

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="config")
    payload = result.to_json()

    schema = next(item for item in result.findings if item.check_id == "config.file")
    provider = next(item for item in result.findings if item.check_id == "config.provider")
    assert schema.auto_repairable is True
    assert "missing policy section" in schema.technical_detail
    assert provider.status == "failed"
    assert "SINGULARITY_API_KEY" in provider.technical_detail
    assert "sk-secret-value" not in payload


def test_data_integrity_reports_broken_memory_jsonl_as_non_destructive(tmp_path):
    paths = resolve_runtime_paths(home=tmp_path / "runtime")
    initialize_runtime(paths)
    memory_entries = tmp_path / ".singularity" / "memory" / "auto" / "entries.jsonl"
    memory_entries.parent.mkdir(parents=True)
    memory_entries.write_text("{broken json\n", encoding="utf-8")

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="data-integrity")

    finding = next(item for item in result.findings if item.check_id == "data_integrity.json_payloads")
    assert finding.status == "failed"
    assert finding.auto_repairable is False
    assert str(memory_entries) in finding.technical_detail


def test_data_integrity_reports_missing_trace_index_as_repairable(tmp_path):
    paths = resolve_runtime_paths(home=tmp_path / "runtime")
    initialize_runtime(paths)
    run_dir = paths.traces_dir / "run_1"
    run_dir.mkdir(parents=True)
    (run_dir / "events.jsonl").write_text("", encoding="utf-8")

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="data_integrity.trace_indexes")

    finding = result.findings[0]
    assert finding.status == "failed"
    assert finding.auto_repairable is True
    assert str(run_dir / "index.json") in finding.technical_detail


def test_schema_check_reports_legacy_manifest_with_migration_hint(tmp_path):
    paths = resolve_runtime_paths(home=tmp_path / "runtime")
    initialize_runtime(paths)
    manifest = json.loads(paths.manifest_file.read_text(encoding="utf-8"))
    manifest["last_migration"] = "000"
    atomic_write_json(paths.manifest_file, manifest)
    atomic_write_json(paths.config_file, default_config(paths))

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="schema")

    finding = next(item for item in result.findings if item.check_id == "schema.migrations")
    assert finding.status == "failed"
    assert finding.auto_repairable is True
    assert finding.details["pending"]
