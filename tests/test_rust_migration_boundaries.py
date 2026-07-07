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
    "raw_response",
    "raw_prompt",
    "raw_arguments",
    "provider_response",
    "policy_decision_id",
    "approval_grant_id",
    "internal_metadata",
    "metadata",
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

RUST_AGENT_HOST_DOC_MARKERS = (
    "Current Python owner",
    "Rust owner after this stage",
    "Parity expectation",
    "Intentional divergence",
    "AgentLoopStatusBridge",
    "SessionStore.create_turn_with_input_and_trace",
)

TURN_LIFECYCLE_DOC_MARKERS = (
    "turn lifecycle",
    "interrupted_requested",
    "PythonSidecarClient::cancel",
    "AgentLoop cancel semantics",
    "SessionStore",
    "trace event",
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
        "src/singularity/agent_host/sidecar.py",
        "docs/singularity.md",
        "docs/architecture/modules/rust-app-server-protocol.md",
        "docs/architecture/rust-agent-host.md",
        "scripts/verify_rust_cli_agent_host.py",
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
    for relative in ("docs/singularity.md", "docs/architecture/modules/rust-app-server-protocol.md"):
        path = repo / relative
        path.write_text(path.read_text(encoding="utf-8").replace(marker, ""), encoding="utf-8")

    result = run_guard(repo, "--changed-file", "crates/app-server/src/lib.rs")

    assert result.returncode == 1
    assert "rust-agent-host-docs-incomplete" in result.stderr
    assert marker in result.stderr


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


def test_guard_rejects_cli_requiring_raw_sidecar_env_as_user_setup(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    cli = repo / "crates/cli/src/main.rs"
    text = cli.read_text(encoding="utf-8")
    text = text.replace("AgentHost::Python", "AgentHost::Disabled")
    cli.write_text(text, encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "raw-sidecar-env-user-setup" in result.stderr


def test_guard_rejects_cli_direct_python_sidecar_invocation(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    cli = repo / "crates/cli/src/main.rs"
    cli.write_text(cli.read_text(encoding="utf-8") + '\nCommand::new("python").arg("-m").arg("singularity.agent_host.sidecar");\n', encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "cli-direct-sidecar-invocation" in result.stderr


@pytest.mark.parametrize(
    ("relative", "old", "new", "violation"),
    (
        (
            "crates/agent/src/lib.rs",
            "SIDECAR_METHOD_RESUME",
            "SIDECAR_METHOD_CONTINUE",
            "sidecar-resume-missing",
        ),
        (
            "crates/agent/src/lib.rs",
            "sidecar_run_params(goal, model)",
            "json!({\"goal\": goal})",
            "sidecar-model-forwarding-missing",
        ),
        (
            "crates/app-server/src/lib.rs",
            "previous_python_session_id",
            "previous_python_run_id",
            "sidecar-resume-session-missing",
        ),
        (
            "crates/app-server/src/lib.rs",
            ".resume_agent(session_id, &goal, model)",
            "client.run_agent(&goal, model)",
            "sidecar-resume-call-missing",
        ),
    ),
)
def test_guard_rejects_missing_sidecar_resume_or_model_forwarding(
    tmp_path: Path, relative: str, old: str, new: str, violation: str
) -> None:
    repo = copy_repo_slice(tmp_path)
    path = repo / relative
    path.write_text(path.read_text(encoding="utf-8").replace(old, new), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert violation in result.stderr


def test_guard_rejects_no_sidecar_fake_agent_delta(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    app_server.write_text(app_server.read_text(encoding="utf-8") + '\nlet _ = "input accepted";\n', encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "fake-agent-delta" in result.stderr


def test_guard_rejects_interrupt_that_updates_status_without_sidecar_cancel(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    app_server = repo / "crates/app-server/src/lib.rs"
    text = app_server.read_text(encoding="utf-8")
    turn_interrupt_start = text.index("    fn turn_interrupt")
    text = text[:turn_interrupt_start] + text[turn_interrupt_start:].replace(".cancel(", ".request_cancel(")
    app_server.write_text(text, encoding="utf-8")

    result = run_guard(repo, "--changed-file", "crates/app-server/src/lib.rs")

    assert result.returncode == 1
    assert "turn-interrupt-missing-sidecar-cancel" in result.stderr


@pytest.mark.parametrize("marker", ("prompt", "provider", "tool", "env"))
def test_guard_rejects_active_sidecar_run_raw_persistence_fields(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    store = repo / "crates/store/src/lib.rs"
    store.write_text(
        store.read_text(encoding="utf-8")
        + f"\ncreate table active_sidecar_runs_extra({marker} text not null);\n",
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "active-sidecar-raw-persistence" in result.stderr
    assert marker in result.stderr


def test_guard_rejects_rust_cli_smoke_copying_full_environment(tmp_path: Path) -> None:
    repo = copy_repo_slice(tmp_path)
    smoke = repo / "scripts/verify_rust_cli_agent_host.py"
    smoke.write_text(smoke.read_text(encoding="utf-8") + "\nenv = os.environ.copy()\n", encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "rust-cli-smoke-env-copy" in result.stderr


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
        ("available: false", "available: true", "native-agent-loop-status-drift"),
        (
            '"strict_command_sandbox"',
            '"strict_sandbox_done"',
            "native-agent-loop-blocker-drift",
        ),
        (
            '"rust_evaluation_runner"',
            '"rust_eval_done"',
            "native-agent-loop-blocker-drift",
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
    cli.write_text(cli.read_text(encoding="utf-8").replace("blockers_empty", "native_ready"), encoding="utf-8")

    result = run_guard(repo)

    assert result.returncode == 1
    assert "native-agent-loop-cli-gate-drift" in result.stderr


@pytest.mark.parametrize(
    ("old", "new"),
    (
        ("NATIVE_AGENT_LOOP_NOT_READY", "NATIVE_AGENT_LOOP_DELAYED"),
        (
            "capability.available && capability.missing_boundaries.is_empty()",
            "capability.available",
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
def test_guard_rejects_sidecar_trace_projection_leaks(tmp_path: Path, marker: str) -> None:
    repo = copy_repo_slice(tmp_path)
    agent = repo / "crates/agent/src/lib.rs"
    text = agent.read_text(encoding="utf-8")
    agent.write_text(
        text.replace('"trace_path": bridge.trace_path,', f'"trace_path": bridge.trace_path,\n        "{marker}": "leak",'),
        encoding="utf-8",
    )

    result = run_guard(repo)

    assert result.returncode == 1
    assert "sidecar-trace-projection-leak" in result.stderr
    assert marker in result.stderr


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
