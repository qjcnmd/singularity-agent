from __future__ import annotations

import json

import singularity.diagnostics as diagnostics
from singularity.diagnostics import DiagnosticRepairResult, DoctorEngine, RepairEngine
from singularity.release.init import initialize_user_data
from singularity.release.models import atomic_write_json
from singularity.release.paths import resolve_user_data_paths


def test_repair_dry_run_does_not_create_user_data_files(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path)

    plan = RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=False)

    assert plan.applied is False
    assert isinstance(plan, DiagnosticRepairResult)
    assert plan.actions
    assert not paths.config_dir.exists()
    assert not paths.config_file.exists()
    assert plan.audit_log_path is None


def test_diagnostics_repair_result_does_not_export_repair_plan_alias() -> None:
    assert hasattr(diagnostics, "DiagnosticRepairResult")
    assert not hasattr(diagnostics, "RepairPlan")


def test_repair_apply_creates_missing_dirs_and_audit_log(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="filesystem")

    plan = RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=True)

    assert plan.applied is True
    assert paths.config_dir.exists()
    assert paths.logs_dir.exists()
    assert plan.audit_log_path == str(paths.logs_dir / "repair-audit.jsonl")
    audit = paths.logs_dir.joinpath("repair-audit.jsonl").read_text(encoding="utf-8")
    assert "filesystem.user_data_dirs" in audit


def test_unfiltered_repair_apply_does_not_create_workspace_suggestions(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    initialize_user_data(paths)
    workspace_state = tmp_path / ".singularity"

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path)
    plan = RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=True)

    assert workspace_state.exists() is False
    assert all(action.check_id != "filesystem.workspace_dirs" for action in plan.actions)
    assert any(
        item["check_id"] == "filesystem.workspace_dirs"
        and item["reason"] == "suggestion_requires_explicit_check"
        for item in plan.blocked_actions
    )


def test_repair_apply_fills_missing_config_fields_without_overwriting_custom(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    initialize_user_data(paths)
    atomic_write_json(paths.config_file, {"schema_version": 1, "component": {"mode": "custom"}})
    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="config.file")

    plan = RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=True)
    config = json.loads(paths.config_file.read_text(encoding="utf-8"))

    assert plan.applied is True
    assert config["component"]["mode"] == "custom"
    assert "policy" not in config
    assert "sandbox" not in config
    assert config["provider"]["api_key_env"] == "SINGULARITY_API_KEY"


def test_repair_apply_rebuilds_missing_trace_index(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    initialize_user_data(paths)
    run_dir = paths.traces_dir / "run_1"
    run_dir.mkdir(parents=True)
    (run_dir / "events.jsonl").write_text("", encoding="utf-8")
    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, check_id="data_integrity.trace_indexes")

    RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=True)

    assert (run_dir / "index.json").exists()


def test_repair_does_not_delete_broken_user_data(tmp_path):
    paths = resolve_user_data_paths(home=tmp_path / "component")
    initialize_user_data(paths)
    broken_memory = tmp_path / ".singularity" / "memory" / "auto" / "entries.jsonl"
    broken_trace = paths.traces_dir / "run_1" / "events.jsonl"
    broken_eval = paths.eval_dir / "report.json"
    for path in (broken_memory, broken_trace, broken_eval):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{broken", encoding="utf-8")

    result = DoctorEngine.default().run(paths=paths, project_root=tmp_path, group="data-integrity")
    plan = RepairEngine().run(result, paths=paths, project_root=tmp_path, apply=True)

    assert plan.actions == []
    assert broken_memory.read_text(encoding="utf-8") == "{broken"
    assert broken_trace.read_text(encoding="utf-8") == "{broken"
    assert broken_eval.read_text(encoding="utf-8") == "{broken"
