from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path("scripts/verify_rust_migration_boundaries.py")

FORBIDDEN_CLI_DEPENDENCIES = (
    "singularity_agent",
    "singularity_model",
    "singularity_store",
    "singularity_tools",
)

FORBIDDEN_DESKTOP_AND_WEB_PATHS = (
    "apps/desktop",
    "src-tauri",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "vite.config.ts",
    "vite.config.js",
    "electron",
    "tauri.conf.json",
)

FORBIDDEN_PYTHON_RUNTIME_NAMES = (
    "RuntimeHost",
    "LocalDaemonRuntime",
    "DesktopTransitionRuntime",
)

FORBIDDEN_TOOL_OBSERVATION_PAYLOAD_MARKERS = (
    "raw_arguments",
    "policy_decision_id",
    "approval_grant_id",
    "internal_metadata",
    "api_key",
    "authorization",
    "password",
    "secret",
    "token",
)

TOOL_OBSERVATION_INTERNAL_FIELDS = (
    "policy_decision_id",
    "approval_grant_id",
    "internal_metadata",
)


def run_guard(repo_root: Path, *extra_args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--repo-root", str(repo_root), *extra_args],
        text=True,
        capture_output=True,
        check=False,
    )


def copy_repo_slice(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    for relative in (
        "crates/core/Cargo.toml",
        "crates/protocol/Cargo.toml",
        "crates/store/Cargo.toml",
        "crates/policy/Cargo.toml",
        "crates/sandbox/Cargo.toml",
        "crates/tools/Cargo.toml",
        "crates/model/Cargo.toml",
        "crates/agent/Cargo.toml",
        "crates/app-server/Cargo.toml",
        "crates/cli/Cargo.toml",
        "crates/app-server/src/lib.rs",
        "crates/protocol/src/lib.rs",
        "crates/tools/src/lib.rs",
        "crates/cli/Cargo.toml",
    ):
        source = Path(relative)
        target = repo / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    (repo / "src/singularity").mkdir(parents=True)
    return repo


def test_current_repository_satisfies_rust_migration_boundaries() -> None:
    result = run_guard(Path.cwd())

    assert result.returncode == 0, result.stderr
    assert "rust migration boundaries verified" in result.stdout


@pytest.mark.parametrize("dependency", FORBIDDEN_CLI_DEPENDENCIES)
def test_guard_rejects_forbidden_cli_dependency(tmp_path: Path, dependency: str) -> None:
    repo = copy_repo_slice(tmp_path)
    manifest = repo / "crates/cli/Cargo.toml"
    manifest.write_text(
        manifest.read_text(encoding="utf-8") + f'\n{dependency} = {{ path = "../{dependency.removeprefix("singularity_")}" }}\n',
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "forbidden-cli-dependency" in result.stderr
    assert dependency in result.stderr


@pytest.mark.parametrize("relative_path", FORBIDDEN_DESKTOP_AND_WEB_PATHS)
def test_guard_rejects_desktop_and_web_startup_files(tmp_path: Path, relative_path: str) -> None:
    repo = copy_repo_slice(tmp_path)
    path = repo / relative_path
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.suffix:
        path.write_text("{}\n", encoding="utf-8")
    else:
        path.mkdir()

    result = run_guard(repo)

    assert result.returncode == 1
    assert "desktop-first-drift" in result.stderr
    assert relative_path in result.stderr


@pytest.mark.parametrize("runtime_name", FORBIDDEN_PYTHON_RUNTIME_NAMES)
def test_guard_rejects_python_runtime_host_names(tmp_path: Path, runtime_name: str) -> None:
    repo = copy_repo_slice(tmp_path)
    runtime_file = repo / "src/singularity/runtime_host.py"
    runtime_file.write_text(f"class {runtime_name}:\n    pass\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "forbidden-python-runtime-host" in result.stderr
    assert runtime_name in result.stderr


def test_guard_rejects_python_core_changes_outside_allowlist(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    core_file = repo / "src/singularity/agent_loop.py"
    core_file.write_text("VALUE = 1\n", encoding="utf-8")

    result = run_guard(repo, "--changed-file", "src/singularity/agent_loop.py")

    assert result.returncode == 1
    assert "python-core-freeze" in result.stderr


@pytest.mark.parametrize("marker", FORBIDDEN_TOOL_OBSERVATION_PAYLOAD_MARKERS)
def test_guard_rejects_tool_observation_model_payload_leaks(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    text = tools.read_text(encoding="utf-8")
    tools.write_text(
        text.replace('"redacted": self.redacted,', f'"redacted": self.redacted,\n            "{marker}": "leak",'),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "tool-observation-model-leak" in result.stderr
    assert marker in result.stderr


@pytest.mark.parametrize("field", TOOL_OBSERVATION_INTERNAL_FIELDS)
def test_guard_rejects_serialized_tool_observation_internal_fields(tmp_path: Path, field: str) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    text = tools.read_text(encoding="utf-8")
    tools.write_text(text.replace(f"    #[serde(skip)]\n    {field}: Option<", f"    {field}: Option<"), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "tool-observation-internal-field-serialized" in result.stderr
    assert field in result.stderr
