from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path("scripts/verify_rust_migration_boundaries.py")
ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"

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

FORBIDDEN_TOOL_RESULT_PAYLOAD_MARKERS = (
    "raw_response",
    "raw_prompt",
    "raw_arguments",
    "provider_response",
    "policy_decision_id",
    "approval_grant_id",
    "audit_metadata",
    "metadata",
    "api_key",
    "authorization",
    "password",
    "secret",
    "token",
)

TOOL_RESULT_INTERNAL_FIELDS = (
    "policy_decision_id",
    "approval_grant_id",
    "audit_metadata",
)

RUST_AGENT_HOST_DOC_MARKERS = (
    "Rust public runtime",
    "Python oracle/parity/dev-only",
    "target-project Python commands",
    "Parity expectation",
    "Intentional divergence",
    "AgentRunStatus",
    "SessionStore.create_turn_with_input_and_trace",
)

def test_python_sidecar_module_is_not_public_runtime_entrypoint() -> None:
    result = subprocess.run(
        [sys.executable, "-m", "singularity.agent_host.sidecar"],
        cwd=ROOT,
        env={**os.environ, "PYTHONPATH": str(SRC)},
        text=True,
        input='{"id":1,"method":"agent/health","params":{}}\n',
        capture_output=True,
        check=False,
        timeout=5,
    )

    assert result.returncode != 0
    combined = result.stdout + result.stderr
    assert "python_sidecar" not in combined
    assert "agent/health" not in combined


TURN_LIFECYCLE_DOC_MARKERS = (
    "turn lifecycle",
    "interrupted_requested",
    "AgentLoop cancel semantics",
    "SessionStore",
    "trace event",
)

PUBLIC_RUNTIME_FORBIDDEN_CASES = (
    ("README.md", "--agent-host", "public-agent-host-surface"),
    ("docs/singularity.md", "agentHost", "public-agent-host-surface"),
    ("docs/architecture/rust-agent-host.md", "SINGULARITY_PYTHON_SIDECAR", "public-agent-host-surface"),
    (
        "docs/architecture/modules/rust-app-server-protocol.md",
        "singularity.agent_host.sidecar",
        "public-agent-host-surface",
    ),
    ("docs/singularity.md", "singularity.cli:main", "public-agent-host-surface"),
    (
        "docs/architecture/modules/rust-app-server-protocol.md",
        "Python CLI remains",
        "public-agent-host-surface",
    ),
    ("crates/cli/src/main.rs", "--agent-host", "public-agent-host-surface"),
    ("crates/protocol/src/lib.rs", "agentHost", "public-agent-host-surface"),
    ("scripts/verify_rust_cli_agent_host.py", "", "public-sidecar-smoke"),
)

FORBIDDEN_LIFECYCLE_NAMES = (
    "RuntimeControlManager",
    "SidecarLifecycleManager",
    "SidecarProcessManager",
    "TransitionRuntime",
    "DesktopTransitionRuntime",
    "LocalDaemonRuntime",
    "MagicBridge",
    "CancellationLifecycleController",
    "ActiveSidecarExecution",
    "SidecarExecutionState",
)

FORBIDDEN_SIDECAR_TRACE_MARKERS = (
    "raw_response",
    "raw_prompt",
    "raw_arguments",
    "provider_response",
    "api_key",
    "authorization",
    "password",
    "secret",
    "token",
    "metadata",
)
STALE_TOOL_RESULT_TYPE_NAME = "Tool" + "Observation"
STALE_CAPABILITY_FIELD = "missing" + "_boundaries"
STALE_PLAN_FIELD = "merge" + "_requirements"


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
        "crates/agent/src/lib.rs",
        "crates/app-server/src/lib.rs",
        "crates/app-server/src/main.rs",
        "crates/cli/src/main.rs",
        "crates/sandbox/src/lib.rs",
        "crates/store/src/lib.rs",
        "crates/protocol/src/lib.rs",
        "crates/tools/src/lib.rs",
        "pyproject.toml",
        "README.md",
        "docs/evaluation/public-representative-task.json",
        "docs/testing.md",
        "docs/singularity.md",
        "docs/architecture/modules/evaluation-benchmark-runner.md",
        "docs/architecture/modules/rust-app-server-protocol.md",
        "docs/architecture/rust-agent-host.md",
        "scripts/export_rust_parity_fixtures.py",
        "tests/fixtures/rust_parity/python_oracle.json",
    ):
        source = Path(relative)
        target = repo / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    (repo / "src/singularity").mkdir(parents=True, exist_ok=True)
    return repo


def test_current_repository_satisfies_rust_migration_boundaries() -> None:
    result = run_guard(Path.cwd())

    assert result.returncode == 0, result.stderr
    assert "rust migration boundaries verified" in result.stdout


@pytest.mark.parametrize("marker", RUST_AGENT_HOST_DOC_MARKERS)
def test_guard_rejects_rust_agent_host_docs_without_logic_map_marker(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    for relative in (
        "docs/singularity.md",
        "docs/architecture/modules/rust-app-server-protocol.md",
        "docs/architecture/rust-agent-host.md",
    ):
        path = repo / relative
        path.write_text(path.read_text(encoding="utf-8").replace(marker, ""), encoding="utf-8")

    result = run_guard(repo, "--changed-file", "docs/singularity.md")

    assert result.returncode == 1
    assert "rust-agent-host-docs-incomplete" in result.stderr


@pytest.mark.parametrize(
    ("relative", "needle"),
    (
        ("docs/evaluation/public-representative-task.json", '"model_visible_verification_command"'),
        ("docs/architecture/rust-agent-host.md", "safe_for_model"),
    ),
)
def test_guard_rejects_rust_public_nonstandard_names(
    tmp_path: Path, relative: str, needle: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    path = repo / relative
    path.write_text(path.read_text(encoding="utf-8") + f"\n{needle}\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "rust-public-naming-drift" in result.stderr


@pytest.mark.parametrize(("relative", "needle", "violation"), PUBLIC_RUNTIME_FORBIDDEN_CASES)
def test_guard_rejects_public_agent_host_or_sidecar_surface(
    tmp_path: Path, relative: str, needle: str, violation: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    path = repo / relative
    if needle:
        path.write_text(path.read_text(encoding="utf-8") + f"\n{needle}\n", encoding="utf-8")
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("# old public sidecar smoke\n", encoding="utf-8")

    result = run_guard(repo, "--changed-file", relative)

    assert result.returncode == 1
    assert violation in result.stderr


def test_guard_scans_public_runtime_surface_without_changed_file_filter(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    readme = repo / "README.md"
    readme.write_text(
        readme.read_text(encoding="utf-8") + "\n--agent-host python\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "public-agent-host-surface" in result.stderr


def test_guard_rejects_python_public_console_scripts(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    pyproject = repo / "pyproject.toml"
    pyproject.write_text(
        pyproject.read_text(encoding="utf-8")
        + '\n[project.scripts]\nsingularity-agent = "singularity.cli:main"\n',
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "public-python-cli-script" in result.stderr


@pytest.mark.parametrize(
    "relative",
    (
        "src/singularity/cli.py",
        "src/singularity/agent_host/__init__.py",
        "src/singularity/agent_host/sidecar.py",
    ),
)
def test_guard_rejects_removed_python_public_runtime_paths(tmp_path: Path, relative: str) -> None:
    repo = copy_repo_slice(tmp_path)
    path = repo / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("# restored old public runtime path\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "public-python-runtime-path" in result.stderr
    assert relative in result.stderr


def test_guard_allows_target_project_python_pytest_commands(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    docs = repo / "docs/testing.md"
    docs.write_text(
        docs.read_text(encoding="utf-8")
        + "\nTarget-project Python commands remain valid through the Rust sandbox: python -m pytest tests/test_app.py\n",
        encoding="utf-8",
    )

    result = run_guard(repo, "--changed-file", "docs/testing.md")

    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize(
    "stale_name",
    (STALE_CAPABILITY_FIELD, STALE_PLAN_FIELD, STALE_TOOL_RESULT_TYPE_NAME),
    ids=("old_capability_field", "old_plan_field", "old_tool_result_type"),
)
def test_guard_rejects_stale_rust_migration_doc_names(tmp_path: Path, stale_name: str) -> None:
    repo = copy_repo_slice(tmp_path)
    docs = repo / "docs/architecture/modules/rust-app-server-protocol.md"
    docs.write_text(docs.read_text(encoding="utf-8") + f"\n{stale_name}\n", encoding="utf-8")

    result = run_guard(repo, "--changed-file", "docs/architecture/modules/rust-app-server-protocol.md")

    assert result.returncode == 1
    assert "rust-agent-host-stale-name" in result.stderr
    assert stale_name in result.stderr


@pytest.mark.parametrize(
    "stale_name",
    ("observation_id", "content_preview", "content_digest", "raw_result_ref"),
)
def test_guard_rejects_stale_rust_parity_fixture_tool_result_names(
    tmp_path: Path, stale_name: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    fixture = repo / "tests/fixtures/rust_parity/python_oracle.json"
    fixture.write_text(
        fixture.read_text(encoding="utf-8") + f'\n{{"{stale_name}": "legacy"}}\n',
        encoding="utf-8",
    )

    result = run_guard(repo, "--changed-file", "tests/fixtures/rust_parity/python_oracle.json")

    assert result.returncode == 1
    assert "rust-parity-fixture-stale-tool-result-name" in result.stderr
    assert stale_name in result.stderr


@pytest.mark.parametrize("marker", TURN_LIFECYCLE_DOC_MARKERS)
def test_guard_rejects_turn_lifecycle_docs_without_required_marker(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    docs = repo / "docs/singularity.md"
    docs.write_text(
        "\n".join(item for item in TURN_LIFECYCLE_DOC_MARKERS if item != marker)
        + "\nlifecycle migration\n",
        encoding="utf-8",
    )

    result = run_guard(repo, "--changed-file", "crates/app-server/src/lib.rs")

    assert result.returncode == 1
    assert "turn-lifecycle-docs-incomplete" in result.stderr
    assert marker in result.stderr


@pytest.mark.parametrize("name", FORBIDDEN_LIFECYCLE_NAMES)
def test_guard_rejects_forbidden_lifecycle_names_in_targeted_files(tmp_path: Path, name: str) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(app_server.read_text(encoding="utf-8") + f"\nstruct {name};\n", encoding="utf-8")

    result = run_guard(repo, "--changed-file", "crates/app-server/src/lib.rs")

    assert result.returncode == 1
    assert "forbidden-lifecycle-name" in result.stderr
    assert name in result.stderr


def test_guard_allows_approved_short_lifecycle_names(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(
        app_server.read_text(encoding="utf-8")
        + "\nstruct TurnRunner;\nstruct SidecarRun;\nstruct RunStatus;\nstruct LifecycleEvent;\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 0, result.stderr


def test_guard_does_not_reject_unrelated_python_domain_forbidden_name(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    python_file = repo / "src/singularity/domain_terms.py"
    python_file.write_text("NAME = 'MagicBridge'\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 0, result.stderr


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


def test_guard_allows_model_runner_context_export_fix_path(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    model_file = repo / "src/singularity/model/runner.py"
    model_file.parent.mkdir(parents=True, exist_ok=True)
    model_file.write_text("ENV_ASSIGNMENT_PATTERN = 'bounded context-export policy'\n", encoding="utf-8")

    result = run_guard(repo, "--changed-file", "src/singularity/model/runner.py")

    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize(
    ("marker", "violation"),
    (
        ("HostWorkspace", "relaxed-sandbox-filesystem-mode"),
        ("Relaxed", "relaxed-sandbox-backend-enforcement"),
        ("pub struct CommandExecutor;", "relaxed-sandbox-executor"),
        ("pub struct PatchExecutor;", "sandbox-host-patch-executor"),
        ("pub fn local_process(", "relaxed-sandbox-command-request"),
        ("pub fn run_local(", "relaxed-sandbox-run-local"),
        ("Command::new(\"python\").spawn()", "direct-sandbox-process-spawn"),
        ("fs::write(\"path\", \"content\")", "direct-sandbox-filesystem-mutation"),
    ),
)
def test_guard_rejects_relaxed_sandbox_process_execution(
    tmp_path: Path, marker: str, violation: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    sandbox = repo / "crates/sandbox/src/lib.rs"
    sandbox.write_text(sandbox.read_text(encoding="utf-8") + f"\n{marker}\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert violation in result.stderr


@pytest.mark.parametrize(
    "marker",
    (
        "PROC_THREAD_ATTRIBUTE_HANDLE_LIST",
        "COMMAND_SENSITIVE_PATH_DENIED",
    ),
)
def test_guard_rejects_incomplete_windows_restricted_token_sandbox(
    tmp_path: Path, marker: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    sandbox = repo / "crates/sandbox/src/lib.rs"
    sandbox.write_text(sandbox.read_text(encoding="utf-8").replace(marker, ""), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "windows-restricted-token-sandbox-incomplete" in result.stderr
    assert marker in result.stderr


@pytest.mark.parametrize(
    "marker",
    (
        "std::process::Command",
        "Command::new(\"python\")",
        ".spawn()",
    ),
)
def test_guard_rejects_tools_command_backend_host_process_spawn(
    tmp_path: Path, marker: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    tools.write_text(tools.read_text(encoding="utf-8") + f"\n{marker}\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "direct-tools-command-process-spawn" in result.stderr
    assert "crates/tools/src/lib.rs" in result.stderr


def test_guard_rejects_tools_command_backend_without_strict_capability_check(
    tmp_path: Path,
) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    tools.write_text(
        tools.read_text(encoding="utf-8").replace(
            "if !capabilities.supports_strict_command_execution()",
            "if false",
        ),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "tools-command-strict-capability-check-missing" in result.stderr


def test_guard_rejects_handwritten_app_server_json_rpc_errors(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/main.rs"
    app_server.write_text(
        app_server.read_text(encoding="utf-8")
        + '\nwriteln!(stdout, "{{\\"error\\":{{\\"message\\":\\"{error}\\"}}}}");\n',
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "handwritten-json-rpc-error" in result.stderr


def test_guard_rejects_fixed_cli_notification_wait(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    cli = repo / "crates/cli/src/main.rs"
    cli.write_text(
        cli.read_text(encoding="utf-8")
        + "\nlet expected_notifications = 4;\nif notifications >= expected_notifications {}\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "fixed-cli-notification-wait" in result.stderr


def test_guard_rejects_cli_notification_drain(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    cli = repo / "crates/cli/src/main.rs"
    cli.write_text(
        cli.read_text(encoding="utf-8")
        + "\nconst EVENT_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);\nfn drain_notifications() {}\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "cli-notification-drain" in result.stderr


def test_guard_rejects_agent_delta_outside_terminal_projection(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    text = app_server.read_text(encoding="utf-8")
    insertion = '        messages.push(AppEvent::item_agent_message_delta("item_fake".to_string(), "fake done".to_string()).to_notification().to_wire_value());\n'
    app_server.write_text(
        text.replace("        messages.extend(self.event_notification(AppEvent::turn_started(&turn)));\n", "        messages.extend(self.event_notification(AppEvent::turn_started(&turn)));\n" + insertion),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "fake-agent-delta" in result.stderr


@pytest.mark.parametrize(
    ("old", "new"),
    (
        ("run_status.status == AgentStatus::Completed", "true"),
        (".filter(|answer| !answer.trim().is_empty())\n            ", ""),
    ),
)
def test_guard_rejects_agent_delta_without_completed_non_empty_gate(
    tmp_path: Path, old: str, new: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(app_server.read_text(encoding="utf-8").replace(old, new), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "fake-agent-delta" in result.stderr


def test_guard_rejects_command_approval_resource_without_scope_normalization(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    agent = repo / "crates/agent/src/lib.rs"
    agent.write_text(
        agent.read_text(encoding="utf-8").replace(
            "let resource =\n        command_scope_resource(&input.argv, &input.sandbox_mode(), &input.network_access());",
            "let resource = fallback.to_string();",
        ),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "command-approval-resource-drift" in result.stderr


def test_guard_rejects_native_gate_without_completed_status(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(
        app_server.read_text(encoding="utf-8").replace(
            "\n        && capability.status == AgentStatus::Completed",
            "",
        ),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "native-agent-loop-app-server-gate-drift" in result.stderr


def test_guard_rejects_active_sidecar_run_persistence(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    store = repo / "crates/store/src/lib.rs"
    store.write_text(
        store.read_text(encoding="utf-8")
        + "\ncreate table active_sidecar_runs(turn_id text primary key);\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "public-sidecar-persistence" in result.stderr


def test_guard_rejects_duplicate_approval_decision_public_api(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    store = repo / "crates/store/src/lib.rs"
    store.write_text(
        store.read_text(encoding="utf-8")
        + "\npub fn record_approval_decision_with_trace() {}\npub fn record_approval_decision() {}\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "duplicate-approval-decision-api" in result.stderr


def test_guard_rejects_approval_decision_without_ledger_or_trace(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    store = repo / "crates/store/src/lib.rs"
    text = store.read_text(encoding="utf-8")
    body_start = text.index("    pub fn record_approval_decision")
    body_end = text.index("    pub fn get_approval_decision", body_start)
    store.write_text(
        text[:body_start]
        + """    pub fn record_approval_decision(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
        reason: &str,
    ) -> StoreResult<()> {
        let changed = self.connection.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![serde_json::to_string(&outcome)?, reason, request_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("approval {request_id}")));
        }
        Ok(())
    }

"""
        + text[body_end:],
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "incomplete-approval-decision-ledger" in result.stderr
    assert "insert_trace" in result.stderr


@pytest.mark.parametrize("manifest_path", ("crates/cli/Cargo.toml", "crates/app-server/Cargo.toml"))
def test_guard_rejects_unused_tokio_crate_dependency(tmp_path: Path, manifest_path: str) -> None:
    repo = copy_repo_slice(tmp_path)
    manifest = repo / manifest_path
    text = manifest.read_text(encoding="utf-8")
    manifest.write_text(text.replace("\n[dev-dependencies]", "\ntokio.workspace = true\n\n[dev-dependencies]"), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "unused-tokio-dependency" in result.stderr
    assert manifest_path in result.stderr


@pytest.mark.parametrize(
    ("old", "new", "violation"),
    (
        ("pub struct AgentLoopCapability", "pub struct AgentLoopState", "native-agent-loop-capability-drift"),
        ("probe_windows_command_sandbox()", "Ok(())", "native-agent-loop-status-drift"),
        (
            "CommandExecutionStatus::Completed",
            "CommandExecutionStatus::SpawnFailed",
            "native-agent-loop-status-drift",
        ),
        (
            "status: AgentStatus::Completed",
            "status: AgentStatus::NotMigrated",
            "native-agent-loop-status-drift",
        ),
        (
            "strict_command_sandbox_unsupported_platform",
            "strict_command_sandbox_pending",
            "native-agent-loop-platform-drift",
        ),
    ),
)
def test_guard_rejects_native_agent_loop_capability_drift(
    tmp_path: Path, old: str, new: str, violation: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    agent = repo / "crates/agent/src/lib.rs"
    agent.write_text(agent.read_text(encoding="utf-8").replace(old, new), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert violation in result.stderr


def test_guard_rejects_old_native_agent_loop_names(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    agent = repo / "crates/agent/src/lib.rs"
    agent.write_text(agent.read_text(encoding="utf-8") + "\npub struct NativeAgentLoop;\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "native-agent-loop-name-drift" in result.stderr


def test_guard_rejects_cli_native_path_without_partial_capability_gate(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    cli = repo / "crates/cli/src/main.rs"
    cli.write_text(
        cli.read_text(encoding="utf-8").replace('blockers == "none"', "true"),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "native-agent-loop-cli-gate-drift" in result.stderr


@pytest.mark.parametrize(
    ("old", "new"),
        (
            ("native_capability_ready(&capability)", "true"),
            (
                "capability.blockers.is_empty()",
                "true",
        ),
    ),
)
def test_guard_rejects_app_server_native_path_without_partial_capability_gate(
    tmp_path: Path, old: str, new: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(app_server.read_text(encoding="utf-8").replace(old, new), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "native-agent-loop-app-server-gate-drift" in result.stderr


@pytest.mark.parametrize("marker", FORBIDDEN_SIDECAR_TRACE_MARKERS)
def test_guard_rejects_sidecar_trace_payload_leaks(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    agent = repo / "crates/agent/src/lib.rs"
    text = agent.read_text(encoding="utf-8")
    agent.write_text(
        text
        + f"""

pub fn sidecar_trace_summary() -> serde_json::Value {{
    serde_json::json!({{"{marker}": "leak"}})
}}
""",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "public-sidecar-trace-summary" in result.stderr
    assert "sidecar-trace-payload-leak" in result.stderr
    assert marker in result.stderr


@pytest.mark.parametrize("marker", FORBIDDEN_TOOL_RESULT_PAYLOAD_MARKERS)
def test_guard_rejects_tool_result_payload_leaks(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    text = tools.read_text(encoding="utf-8")
    tools.write_text(
        text.replace('"redacted": self.redacted,', f'"redacted": self.redacted,\n            "{marker}": "leak",'),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "tool-result-model-leak" in result.stderr
    assert marker in result.stderr


@pytest.mark.parametrize("field", TOOL_RESULT_INTERNAL_FIELDS)
def test_guard_rejects_serialized_tool_result_internal_fields(tmp_path: Path, field: str) -> None:
    repo = copy_repo_slice(tmp_path)
    tools = repo / "crates/tools/src/lib.rs"
    text = tools.read_text(encoding="utf-8")
    tools.write_text(text.replace(f"    #[serde(skip)]\n    {field}: Option<", f"    {field}: Option<"), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "tool-result-internal-field-serialized" in result.stderr
    assert field in result.stderr
