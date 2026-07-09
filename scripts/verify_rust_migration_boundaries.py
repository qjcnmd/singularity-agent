#!/usr/bin/env python3
"""Guard Rust public runtime migration boundaries."""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

FORBIDDEN_CLI_DEPENDENCIES = {
    "singularity_agent",
    "singularity_model",
    "singularity_store",
    "singularity_tools",
}

ALLOWED_CRATE_DEPENDENCIES = {
    "crates/core/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json", "thiserror", "time", "uuid"},
        "dev-dependencies": set(),
    },
    "crates/protocol/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json", "singularity_core", "singularity_policy"},
        "dev-dependencies": {"singularity_tools"},
    },
    "crates/store/Cargo.toml": {
        "dependencies": {
            "rusqlite",
            "schemars",
            "serde",
            "serde_json",
            "singularity_core",
            "singularity_policy",
            "singularity_protocol",
            "thiserror",
            "uuid",
        },
        "dev-dependencies": {"tempfile"},
    },
    "crates/policy/Cargo.toml": {
        "dependencies": {"schemars", "serde"},
        "dev-dependencies": {"serde_json"},
    },
    "crates/sandbox/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json", "singularity_core"},
        "dev-dependencies": {"tempfile"},
    },
    "crates/tools/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json", "singularity_core", "singularity_sandbox"},
        "dev-dependencies": set(),
    },
    "crates/model/Cargo.toml": {
        "dependencies": {"reqwest", "schemars", "serde", "serde_json", "thiserror"},
        "dev-dependencies": set(),
    },
    "crates/agent/Cargo.toml": {
        "dependencies": {
            "schemars",
            "serde",
            "serde_json",
            "singularity_model",
            "singularity_policy",
            "singularity_tools",
            "thiserror",
        },
        "dev-dependencies": {"tempfile"},
    },
    "crates/app-server/Cargo.toml": {
        "dependencies": {
            "schemars",
            "serde",
            "serde_json",
            "singularity_agent",
            "singularity_core",
            "singularity_model",
            "singularity_policy",
            "singularity_protocol",
            "singularity_store",
            "singularity_tools",
            "thiserror",
        },
        "dev-dependencies": {"tempfile"},
    },
    "crates/cli/Cargo.toml": {
        "dependencies": {"clap", "serde_json", "singularity_core", "singularity_protocol"},
        "dev-dependencies": {"assert_cmd", "tempfile"},
    },
}

ALLOWED_PYTHON_MIGRATION_PATHS = {
    "src/singularity/agent_host",
    "src/singularity/diagnostics",
    "src/singularity/evaluation/manifests.py",
    "src/singularity/model/runner.py",
    "scripts/export_rust_parity_fixtures.py",
    "tests/test_rust_parity_fixtures.py",
    "tests/test_rust_migration_boundaries.py",
    "tests/test_model_runner.py",
}

FORBIDDEN_PYTHON_RUNTIME_NAMES = {
    "RuntimeHost",
    "LocalDaemonRuntime",
    "DesktopTransitionRuntime",
}

FORBIDDEN_DESKTOP_PATHS = {
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
}

TOOL_RESULT_PAYLOAD_FORBIDDEN = {
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
}

RUST_AGENT_HOST_DOC_MARKERS = {
    "Rust public runtime",
    "Python oracle/parity/dev-only",
    "target-project Python commands",
    "Parity expectation",
    "Intentional divergence",
    "AgentRunStatus",
    "SessionStore.create_turn_with_input_and_trace",
}

TURN_LIFECYCLE_DOC_MARKERS = {
    "turn lifecycle",
    "interrupted_requested",
    "AgentLoop cancel semantics",
    "SessionStore",
    "trace event",
}

PUBLIC_RUNTIME_SURFACE_TARGETS = {
    "README.md",
    "docs/INSTALL.md",
    "docs/PLUGIN_MANAGEMENT.md",
    "docs/testing.md",
    "docs/singularity.md",
    "docs/architecture/rust-agent-host.md",
    "docs/architecture/modules/rust-app-server-protocol.md",
    "docs/architecture/modules/sandbox-isolation.md",
    "crates/protocol/src/lib.rs",
    "crates/cli/src/main.rs",
    "crates/app-server/src/lib.rs",
}

PUBLIC_RUNTIME_FORBIDDEN_MARKERS = {
    "agentHost": "public turn/start params must not expose an agent host selector",
    "agent_host": "public TurnStartParams must not expose an agent host selector",
    "--agent-host": "public CLI must not expose a Python/Rust backend selector",
    "AgentHost::Native": "public Rust enum variants must not expose backend selection",
    "AgentHost::Python": "public Rust enum variants must not expose backend selection",
    "SINGULARITY_PYTHON_SIDECAR": "ordinary users must not configure Python sidecar runtime selection",
    "singularity.agent_host.sidecar": "public docs or CLI must not expose the Python sidecar module path",
    "verify_rust_cli_agent_host.py": "public verification must not require the old Python sidecar smoke",
    "singularity-agent": "public docs must not present the Python CLI as a user-facing runtime command",
}

PUBLIC_SIDECAR_SMOKE_SCRIPT = "scripts/verify_rust_cli_agent_host.py"

FORBIDDEN_LIFECYCLE_NAMES = {
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
}
STALE_TOOL_RESULT_TYPE_NAME = "Tool" + "Observation"
STALE_INTERNAL_METADATA_FIELD = "internal" + "_metadata"
STALE_TOOL_OUTPUT_FIXTURE_KEY = "tool_protocol_result" + "_envelope"
STALE_CAPABILITY_FIELD = "missing" + "_boundaries"
STALE_PLAN_FIELD = "merge" + "_requirements"
RUST_PUBLIC_NAMING_TARGETS = (
    "docs/evaluation/public-representative-task.json",
    "docs/architecture/modules/rust-app-server-protocol.md",
    "docs/architecture/rust-agent-host.md",
    "docs/singularity.md",
)
RUST_PUBLIC_FORBIDDEN_NAMING = (
    "model" + "_visible",
    "model-visible",
    "safe" + "_for_model",
)

LIFECYCLE_NAME_TARGETS = (
    "docs/singularity.md",
    "docs/architecture/modules/rust-app-server-protocol.md",
    "docs/architecture/rust-agent-host.md",
    "crates/app-server/src/lib.rs",
    "crates/agent/src/lib.rs",
    "crates/cli/src/main.rs",
    "crates/store/src/lib.rs",
)

SIDECAR_TRACE_PROJECTION_FORBIDDEN = {
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
}


@dataclass(frozen=True)
class Violation:
    code: str
    path: str
    detail: str


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", default=str(REPO_ROOT), help="Repository root to check.")
    parser.add_argument(
        "--changed-file",
        action="append",
        default=[],
        help="Changed file path to check for Python freeze violations. Defaults to git working tree changes.",
    )
    args = parser.parse_args()

    repo_root = Path(args.repo_root).resolve()
    changed_files = [Path(item).as_posix() for item in args.changed_file] or _git_changed_files(repo_root)

    violations: list[Violation] = []
    violations.extend(_check_crate_dependencies(repo_root))
    violations.extend(_check_cli_dependencies(repo_root))
    violations.extend(_check_python_freeze(repo_root, changed_files))
    violations.extend(_check_forbidden_python_runtime_names(repo_root))
    violations.extend(_check_forbidden_desktop_paths(repo_root))
    violations.extend(_check_agent_loop_status(repo_root))
    violations.extend(_check_rust_agent_host_docs(repo_root))
    violations.extend(_check_public_runtime_surface(repo_root, changed_files))
    violations.extend(_check_python_cli_public_scripts(repo_root))
    violations.extend(_check_rust_public_naming(repo_root))
    violations.extend(_check_turn_lifecycle_docs(repo_root))
    violations.extend(_check_forbidden_lifecycle_names(repo_root))
    violations.extend(_check_no_fake_agent_delta(repo_root))
    violations.extend(_check_turn_interrupt_cancel_boundary(repo_root, changed_files))
    violations.extend(_check_active_sidecar_run_persistence(repo_root))
    violations.extend(_check_sidecar_trace_payload(repo_root))
    violations.extend(_check_tool_result_payload(repo_root))
    violations.extend(_check_command_approval_resource_boundary(repo_root))
    violations.extend(_check_tools_command_backend_boundary(repo_root))
    violations.extend(_check_rust_parity_fixture_names(repo_root))
    violations.extend(_check_sandbox_command_boundary(repo_root))
    violations.extend(_check_app_server_transport_errors(repo_root))
    violations.extend(_check_cli_protocol_read_loop(repo_root))
    violations.extend(_check_approval_decision_boundary(repo_root))
    violations.extend(_check_unused_tokio_dependencies(repo_root))

    if violations:
        for violation in violations:
            print(f"{violation.code}: {violation.path}: {violation.detail}", file=sys.stderr)
        return 1

    print("rust migration boundaries verified")
    return 0


def _check_crate_dependencies(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    crate_manifests = sorted((repo_root / "crates").glob("*/Cargo.toml"))
    for manifest_path in crate_manifests:
        relative = _relative(manifest_path, repo_root)
        allowed = ALLOWED_CRATE_DEPENDENCIES.get(relative)
        if allowed is None:
            violations.append(Violation("unknown-crate-manifest", relative, "crate manifest is not in migration allowlist"))
            continue
        manifest = _load_toml(manifest_path)
        for section in ("dependencies", "dev-dependencies"):
            actual = set((manifest.get(section) or {}).keys())
            extra = actual - allowed[section]
            missing = allowed[section] - actual
            for name in sorted(extra):
                violations.append(
                    Violation(
                        "unexplained-crate-dependency",
                        relative,
                        f"{section}.{name} is not in the documented M0 dependency allowlist",
                    )
                )
            for name in sorted(missing):
                violations.append(
                    Violation(
                        "missing-crate-dependency",
                        relative,
                        f"{section}.{name} was removed without updating the migration boundary allowlist",
                    )
                )
    return violations


def _check_cli_dependencies(repo_root: Path) -> list[Violation]:
    manifest_path = repo_root / "crates" / "cli" / "Cargo.toml"
    manifest = _load_toml(manifest_path)
    dependencies = set((manifest.get("dependencies") or {}).keys())
    dependencies.update((manifest.get("dev-dependencies") or {}).keys())
    forbidden = sorted(dependencies & FORBIDDEN_CLI_DEPENDENCIES)
    return [
        Violation("forbidden-cli-dependency", _relative(manifest_path, repo_root), f"crates/cli depends on {name}")
        for name in forbidden
    ]


def _check_python_freeze(repo_root: Path, changed_files: list[str]) -> list[Violation]:
    violations: list[Violation] = []
    for changed in changed_files:
        path = Path(changed).as_posix()
        if not path.startswith("src/singularity/") or not path.endswith(".py"):
            continue
        if _is_allowed_python_migration_path(path):
            continue
        violations.append(
            Violation(
                "python-core-freeze",
                path,
                "Python runtime changes must stay in sidecar/oracle/fixture/parity/diagnostics allowlist during Rust migration",
            )
        )
    return violations


def _check_forbidden_python_runtime_names(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    src_root = repo_root / "src" / "singularity"
    for path in sorted(src_root.rglob("*.py")):
        relative = _relative(path, repo_root)
        text = path.read_text(encoding="utf-8")
        for name in sorted(FORBIDDEN_PYTHON_RUNTIME_NAMES):
            if name in text:
                violations.append(
                    Violation(
                        "forbidden-python-runtime-host",
                        relative,
                        f"{name} would move product runtime behavior back into Python",
                    )
                )
    return violations


def _check_forbidden_desktop_paths(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for relative in sorted(FORBIDDEN_DESKTOP_PATHS):
        if (repo_root / relative).exists():
            violations.append(
                Violation(
                    "desktop-first-drift",
                    relative,
                    "desktop/web files are blocked until the CLI-first Rust agent host is usable",
                )
            )
    return violations


def _check_agent_loop_status(repo_root: Path) -> list[Violation]:
    app_server = repo_root / "crates" / "app-server" / "src" / "lib.rs"
    agent = repo_root / "crates" / "agent" / "src" / "lib.rs"
    protocol = repo_root / "crates" / "protocol" / "src" / "lib.rs"
    text = app_server.read_text(encoding="utf-8")
    agent_text = agent.read_text(encoding="utf-8")
    if "NativeAgentLoop" in agent_text:
        return [
            Violation(
                "native-agent-loop-name-drift",
                _relative(agent, repo_root),
                "temporary NativeAgentLoop names must not remain after AgentLoop exists",
            )
        ]
    if "pub struct AgentLoopCapability" not in agent_text or "pub struct AgentLoop<" not in agent_text:
        return [
            Violation(
                "native-agent-loop-capability-drift",
                _relative(agent, repo_root),
                "AgentLoopCapability and AgentLoop must exist for the native AgentLoop path",
            )
        ]
    if (
        "#[cfg(windows)]" not in agent_text
        or "windows_agent_loop_capability" not in agent_text
        or "probe_windows_command_sandbox()" not in agent_text
        or "WindowsRestrictedTokenSandboxBackend::new().execute(&request)" not in agent_text
        or "CommandExecutionStatus::Completed" not in agent_text
        or "CommandSemanticStatus::Succeeded" not in agent_text
        or "STRICT_COMMAND_SANDBOX_PROBE_FAILED" not in agent_text
        or "available: true" not in agent_text
        or "status: AgentStatus::Completed" not in agent_text
    ):
        return [
            Violation(
                "native-agent-loop-status-drift",
                _relative(agent, repo_root),
                "AgentLoopCapability must report completed native cutover only after a Windows sandbox probe succeeds",
            )
        ]
    if (
        "#[cfg(not(windows))]" not in agent_text
        or "available: false" not in agent_text
        or "status: AgentStatus::Blocked" not in agent_text
        or "strict_command_sandbox_unsupported_platform" not in agent_text
    ):
        return [
            Violation(
                "native-agent-loop-platform-drift",
                _relative(agent, repo_root),
                "AgentLoopCapability must fail closed off Windows until a strict command sandbox backend exists",
            )
        ]
    if "blockers: Vec::new()" not in agent_text or "blockers: vec![blocker]" not in agent_text:
        return [
            Violation(
                "native-agent-loop-status-drift",
                _relative(agent, repo_root),
                "AgentLoopCapability must keep separate success and fail-closed blocker paths on Windows",
            )
        ]
    cli_text = (repo_root / "crates" / "cli" / "src" / "main.rs").read_text(encoding="utf-8")
    if '"blockers"' not in cli_text or "native_agent_loop_blockers" not in cli_text or 'blockers == "none"' not in cli_text:
        return [
            Violation(
                "native-agent-loop-cli-gate-drift",
                "crates/cli/src/main.rs",
                "CLI native runtime must reject partial AgentLoop capability until blockers are empty",
            )
        ]
    if "native_capability_ready(&capability)" not in text or "native_agent_loop_unavailable_message(&capability)" not in text:
        return [
            Violation(
                "native-agent-loop-app-server-gate-drift",
                _relative(app_server, repo_root),
                "app-server must reject public turn/start while AgentLoopCapability blockers remain",
            )
        ]
    if (
        "capability.available" not in text
        or "capability.blockers.is_empty()" not in text
        or "capability.status == AgentStatus::Completed" not in text
    ):
        return [
            Violation(
                "native-agent-loop-app-server-gate-drift",
                _relative(app_server, repo_root),
                "app-server native gate must require available capability, completed status, and empty blockers",
            )
        ]
    protocol_text = protocol.read_text(encoding="utf-8")
    if "agent_loop_status" not in protocol_text:
        return [
            Violation(
                "agent-loop-status-missing",
                _relative(protocol, repo_root),
                "Turn must expose agent_loop_status while AgentLoop is not migrated",
            )
        ]
    return []


def _check_public_runtime_surface(repo_root: Path, changed_files: list[str]) -> list[Violation]:
    violations: list[Violation] = []
    if (repo_root / PUBLIC_SIDECAR_SMOKE_SCRIPT).exists():
        violations.append(
            Violation(
                "public-sidecar-smoke",
                PUBLIC_SIDECAR_SMOKE_SCRIPT,
                PUBLIC_RUNTIME_FORBIDDEN_MARKERS["verify_rust_cli_agent_host.py"],
            )
        )
    for relative in sorted(PUBLIC_RUNTIME_SURFACE_TARGETS):
        path = Path(relative).as_posix()
        target = repo_root / path
        if not target.exists():
            continue
        text = target.read_text(encoding="utf-8")
        for marker, detail in sorted(PUBLIC_RUNTIME_FORBIDDEN_MARKERS.items()):
            if marker in text:
                violations.append(Violation("public-agent-host-surface", path, detail))
    return violations


def _check_python_cli_public_scripts(repo_root: Path) -> list[Violation]:
    path = repo_root / "pyproject.toml"
    if not path.exists():
        return []
    scripts = tomllib.loads(path.read_text(encoding="utf-8")).get("project", {}).get("scripts", {})
    if not scripts:
        return []
    return [
        Violation(
            "public-python-cli-script",
            _relative(path, repo_root),
            "Python package must not install public CLI console scripts during Rust-only public runtime migration",
        )
    ]


def _check_rust_agent_host_docs(repo_root: Path) -> list[Violation]:
    docs = [
        repo_root / "docs" / "singularity.md",
        repo_root / "docs" / "architecture" / "modules" / "rust-app-server-protocol.md",
        repo_root / "docs" / "architecture" / "rust-agent-host.md",
    ]
    doc_texts = {path: path.read_text(encoding="utf-8") for path in docs}
    violations = [
        Violation(
            "rust-agent-host-docs-incomplete",
            "docs/singularity.md,docs/architecture/modules/rust-app-server-protocol.md,docs/architecture/rust-agent-host.md",
            f"Rust CLI-first migration docs must include marker: {marker}",
        )
        for marker in sorted(RUST_AGENT_HOST_DOC_MARKERS)
        if marker not in "\n".join(doc_texts.values())
    ]
    stale_names = (STALE_CAPABILITY_FIELD, STALE_PLAN_FIELD, STALE_TOOL_RESULT_TYPE_NAME)
    for path in docs:
        text = _rust_migration_doc_text(path, repo_root, doc_texts[path])
        for name in stale_names:
            if name in text:
                violations.append(
                    Violation(
                        "rust-agent-host-stale-name",
                        _relative(path, repo_root),
                        f"Rust migration docs must use current public names, not {name}",
                    )
                )
    return violations


def _rust_migration_doc_text(path: Path, repo_root: Path, text: str) -> str:
    if _relative(path, repo_root) != "docs/singularity.md":
        return text
    start = text.find("## Rust Agent Host")
    if start < 0:
        return text
    end = text.find("\n---\n\n```", start + 1)
    return text[start:] if end < 0 else text[start:end]


def _check_rust_public_naming(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for relative in RUST_PUBLIC_NAMING_TARGETS:
        path = repo_root / relative
        if not path.exists():
            continue
        text = _rust_migration_doc_text(path, repo_root, path.read_text(encoding="utf-8"))
        for name in RUST_PUBLIC_FORBIDDEN_NAMING:
            if name in text:
                violations.append(
                    Violation(
                        "rust-public-naming-drift",
                        relative,
                        f"Rust migration public surface must use neutral names such as smoke_command, not {name}",
                    )
                )
    return violations


def _check_turn_lifecycle_docs(repo_root: Path) -> list[Violation]:
    docs = [
        repo_root / "docs" / "singularity.md",
        repo_root / "docs" / "architecture" / "modules" / "rust-app-server-protocol.md",
        repo_root / "docs" / "architecture" / "rust-agent-host.md",
    ]
    violations: list[Violation] = []
    for path in docs:
        text = path.read_text(encoding="utf-8")
        lowered = text.lower()
        if "turn lifecycle" not in lowered and "lifecycle migration" not in lowered:
            continue
        violations.extend(
            Violation(
                "turn-lifecycle-docs-incomplete",
                _relative(path, repo_root),
                f"turn lifecycle migration docs must include marker: {marker}",
            )
            for marker in sorted(TURN_LIFECYCLE_DOC_MARKERS)
            if marker not in text
        )
    return violations


def _check_forbidden_lifecycle_names(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for relative in LIFECYCLE_NAME_TARGETS:
        path = repo_root / relative
        if not path.exists():
            continue
        text = path.read_text(encoding="utf-8")
        for name in sorted(FORBIDDEN_LIFECYCLE_NAMES):
            if name in text:
                violations.append(
                    Violation(
                        "forbidden-lifecycle-name",
                        relative,
                        f"{name} is a verbose or invented lifecycle name; use approved vocabulary",
                    )
                )
    return violations


def _check_no_fake_agent_delta(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "app-server" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    violations: list[Violation] = []
    terminal_body = _extract_rust_function_body(text, "agent_terminal_item_events")
    if terminal_body is None:
        return [
            Violation(
                "fake-agent-delta",
                relative,
                "agent_terminal_item_events must own assistant delta projection",
            )
        ]
    if text.count("item_agent_message_delta(") != terminal_body.count("item_agent_message_delta("):
        violations.append(
            Violation(
                "fake-agent-delta",
                relative,
                "assistant delta events must only be emitted by agent_terminal_item_events",
            )
        )
    if "agent_completed_delta(run_status)" not in terminal_body:
        violations.append(
            Violation(
                "fake-agent-delta",
                relative,
                "agent_terminal_item_events must gate assistant delta through agent_completed_delta(run_status)",
            )
        )
    delta_body = _extract_rust_function_body(text, "agent_completed_delta")
    required_delta_markers = (
        "run_status.status == AgentStatus::Completed",
        ".final_answer",
        ".filter(|answer| !answer.trim().is_empty())",
        "redact_app_server_text",
    )
    if delta_body is None:
        violations.append(
            Violation(
                "fake-agent-delta",
                relative,
                "agent_completed_delta must gate completed non-empty final answers before assistant delta projection",
            )
        )
    else:
        for marker in required_delta_markers:
            if marker not in delta_body:
                violations.append(
                    Violation(
                        "fake-agent-delta",
                        relative,
                        f"agent_completed_delta must require marker before assistant delta projection: {marker}",
                    )
                )
    turn_start_body = _extract_rust_function_body(text, "turn_start")
    if turn_start_body is None:
        violations.append(
            Violation(
                "fake-agent-delta",
                relative,
                "turn_start must keep native gate before turn creation",
            )
        )
    else:
        gate_index = turn_start_body.find("native_capability_ready(&capability)")
        create_index = turn_start_body.find("create_turn_with_input_and_trace")
        if gate_index == -1 or create_index == -1 or gate_index > create_index:
            violations.append(
                Violation(
                    "fake-agent-delta",
                    relative,
                    "native AgentLoop rejection must happen before durable turn creation or assistant delta projection",
                )
            )
    return violations


def _check_command_approval_resource_boundary(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "agent" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    violations: list[Violation] = []
    permission_body = _extract_rust_function_body(text, "permission_resources_for_tool")
    command_body = _extract_rust_function_body(text, "command_permission_resources")
    if permission_body is None or "call.tool_name == TOOL_COMMAND" not in permission_body or "command_permission_resources(&call.arguments, &call.tool_name)" not in permission_body:
        violations.append(
            Violation(
                "command-approval-resource-drift",
                relative,
                "command tool approvals must route through command_permission_resources instead of a generic builtin.command resource",
            )
        )
    if command_body is None or "command_scope_resource(" not in command_body or ".sandbox_mode()" not in command_body or ".network_access()" not in command_body:
        violations.append(
            Violation(
                "command-approval-resource-drift",
                relative,
                "command_permission_resources must derive the approval resource from argv plus sandbox/network scope",
            )
        )
    return violations


def _check_turn_interrupt_cancel_boundary(repo_root: Path, changed_files: list[str]) -> list[Violation]:
    return []


def _check_active_sidecar_run_persistence(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for relative in ("crates/store/src/lib.rs", "crates/app-server/src/lib.rs"):
        path = repo_root / relative
        if path.exists() and "active_sidecar_runs" in path.read_text(encoding="utf-8"):
            violations.append(
                Violation(
                    "public-sidecar-persistence",
                    relative,
                    "public runtime must not retain active Python sidecar lifecycle persistence",
                )
            )
    return violations


def _check_sidecar_trace_payload(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "agent" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    body = _extract_rust_function_body(text, "sidecar_trace_summary")
    if body is None:
        return []
    violations = [
        Violation(
            "public-sidecar-trace-summary",
            _relative(path, repo_root),
            "Rust public agent runtime must not expose Python sidecar trace summaries",
        )
    ]
    lowered = body.lower()
    violations.extend(
        Violation(
            "sidecar-trace-payload-leak",
            _relative(path, repo_root),
            f"sidecar_trace_summary references forbidden marker {marker}",
        )
        for marker in sorted(SIDECAR_TRACE_PROJECTION_FORBIDDEN)
        if marker in lowered
    )
    return violations


def _check_tool_result_payload(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "tools" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    body = _extract_rust_function_body(text, "to_message_payload")
    violations: list[Violation] = []
    if body is None:
        return [Violation("tool-result-payload-missing", _relative(path, repo_root), "to_message_payload not found")]
    lowered = body.lower()
    for marker in sorted(TOOL_RESULT_PAYLOAD_FORBIDDEN):
        if marker in lowered:
            violations.append(
                Violation(
                    "tool-result-model-leak",
                    _relative(path, repo_root),
                    f"to_message_payload references model-forbidden marker {marker}",
                )
            )
    for field in ("policy_decision_id", "approval_grant_id", "audit_metadata"):
        if f"#[serde(skip)]\n    {field}" not in text:
            violations.append(
                Violation(
                    "tool-result-internal-field-serialized",
                    _relative(path, repo_root),
                    f"{field} must stay serde-skipped",
                )
            )
    return violations


def _check_rust_parity_fixture_names(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    fixture = repo_root / "tests" / "fixtures" / "rust_parity" / "python_oracle.json"
    stale_names = (
        STALE_TOOL_RESULT_TYPE_NAME,
        "observation_id",
        "content_preview",
        "content_digest",
        "raw_result_ref",
        STALE_TOOL_OUTPUT_FIXTURE_KEY,
        STALE_INTERNAL_METADATA_FIELD,
    )
    if fixture.exists():
        text = fixture.read_text(encoding="utf-8")
        for name in stale_names:
            if name in text:
                violations.append(
                    Violation(
                        "rust-parity-fixture-stale-tool-result-name",
                        _relative(fixture, repo_root),
                        f"Rust-facing parity fixture/export must use ToolResult/ToolOutput names, not {name}",
                    )
                )
    exporter = repo_root / "scripts" / "export_rust_parity_fixtures.py"
    if exporter.exists():
        text = exporter.read_text(encoding="utf-8")
        for name in (
            STALE_TOOL_RESULT_TYPE_NAME,
            STALE_TOOL_OUTPUT_FIXTURE_KEY,
            STALE_INTERNAL_METADATA_FIELD,
        ):
            if name not in text:
                continue
            violations.append(
                Violation(
                    "rust-parity-fixture-stale-tool-result-name",
                    _relative(exporter, repo_root),
                    f"Rust-facing parity exporter must not expose stale tool result terminology: {name}",
                )
            )
        if '"failed_result": _rust_tool_output(failed_tool_result)' not in text:
            violations.append(
                Violation(
                    "rust-parity-fixture-stale-tool-result-name",
                    _relative(exporter, repo_root),
                    "tool_repair.failed_result must be exported with Rust ToolOutput field names",
                )
            )
    return violations


def _check_tools_command_backend_boundary(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "tools" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    violations: list[Violation] = []
    if any(marker in text for marker in ("Command::new", ".spawn()", "std::process::Command", "std::process::{")):
        violations.append(
            Violation(
                "direct-tools-command-process-spawn",
                relative,
                "tools command backend boundary must delegate to SandboxBackend and must not spawn host processes",
            )
        )
    body = _extract_rust_function_body(text, "command")
    if body is None or "supports_strict_command_execution" not in body:
        violations.append(
            Violation(
                "tools-command-strict-capability-check-missing",
                relative,
                "WorkspaceTools::command must reject command backends without strict sandbox capabilities",
            )
        )
    return violations


def _check_sandbox_command_boundary(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "sandbox" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    checks = (
        ("relaxed-sandbox-filesystem-mode", "HostWorkspace", "SandboxFilesystemMode must not expose host-workspace/no-sandbox mode during native cutover"),
        ("relaxed-sandbox-backend-enforcement", "Relaxed", "SandboxBackendEnforcement must not expose relaxed backend mode during native cutover"),
        ("relaxed-sandbox-executor", "pub struct CommandExecutor", "sandbox crate must not expose a local process executor during native cutover"),
        (
            "relaxed-sandbox-command-request",
            "pub fn local_process",
            "CommandRequest must not expose a relaxed host local-process constructor",
        ),
        ("relaxed-sandbox-run-local", "pub fn run_local", "sandbox crate must not expose run_local without a strict backend"),
        ("sandbox-host-patch-executor", "pub struct PatchExecutor", "sandbox crate must not expose host filesystem mutation executor during native cutover"),
    )
    violations = [
        Violation(code, relative, detail)
        for code, marker, detail in checks
        if marker in text
    ]
    if any(marker in text for marker in ("Command::new", ".spawn()", "std::process::Command", "std::process::{")):
        violations.append(
            Violation(
                "direct-sandbox-process-spawn",
                relative,
                "sandbox crate must not use std::process or local process fallback; command execution must stay inside a Rust-owned SandboxBackend",
            )
        )
    if "WindowsRestrictedTokenSandboxBackend" in text:
        required_markers = {
            "CreateRestrictedToken": "restricted token",
            "SetTokenInformation": "low-integrity token",
            "TokenIntegrityLevel": "low-integrity token",
            "SECURITY_MANDATORY_LOW_RID": "low-integrity token",
            "CreateJobObjectW": "Job Object",
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE": "process-tree cleanup",
            "PROC_THREAD_ATTRIBUTE_HANDLE_LIST": "stdio handle allowlist",
            "command_request_denial": "path admission",
            "COMMAND_SENSITIVE_PATH_DENIED": "sensitive path deny",
            "COMMAND_READ_ONLY_WRITE_DENIED": "read-only write deny",
            "CommandExecutionStatus::Unsupported": "unsupported status",
        }
        for marker, reason in required_markers.items():
            if marker not in text:
                violations.append(
                    Violation(
                        "windows-restricted-token-sandbox-incomplete",
                        relative,
                        f"Windows restricted-token sandbox backend must include {reason} marker: {marker}",
                    )
                )
    if any(marker in text for marker in ("fs::write", "std::fs::write", "fs::remove_file", "std::fs::remove_file")):
        violations.append(
            Violation(
                "direct-sandbox-filesystem-mutation",
                relative,
                "sandbox crate must not mutate host filesystem paths during native cutover",
            )
        )
    return violations


def _check_app_server_transport_errors(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "app-server" / "src" / "main.rs"
    text = path.read_text(encoding="utf-8")
    handwritten_error_markers = (
        '\\"error\\"',
        '"{{\\"error\\":',
        '{{"error":',
        '{"error":',
    )
    if not any(marker in text for marker in handwritten_error_markers):
        return []
    return [
        Violation(
            "handwritten-json-rpc-error",
            _relative(path, repo_root),
            "stdio transport errors must be serialized through serde_json/JsonRpcMessage, not hand-written JSON strings",
        )
    ]


def _check_cli_protocol_read_loop(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "cli" / "src" / "main.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    violations: list[Violation] = []
    if "expected_notifications" in text:
        violations.append(
            Violation(
                "fixed-cli-notification-wait",
                relative,
                "CLI requests must complete on matching response id without fixed notification counts",
            )
        )
    if "drain_notifications" in text or "EVENT_DRAIN_TIMEOUT" in text:
        violations.append(
            Violation(
                "cli-notification-drain",
                relative,
                "CLI must not drain post-response messages; it should return on the matching response id",
            )
        )
    return violations


def _check_approval_decision_boundary(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "store" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    violations: list[Violation] = []
    if "record_approval_decision_with_trace" in text:
        violations.append(
            Violation(
                "duplicate-approval-decision-api",
                relative,
                "approval decisions must have one public durable ledger + trace writer",
            )
        )
    if text.count("pub fn record_approval_decision") != 1:
        violations.append(
            Violation(
                "duplicate-approval-decision-api",
                relative,
                "store must expose exactly one public record_approval_decision function",
            )
        )
    body = _extract_rust_function_body(text, "record_approval_decision")
    if body is None:
        violations.append(
            Violation(
                "missing-approval-decision-api",
                relative,
                "record_approval_decision public durable writer not found",
            )
        )
        return violations
    missing = []
    if "approval_decisions" not in body:
        missing.append("approval_decisions insert")
    if "insert_trace" not in body:
        missing.append("insert_trace")
    if missing:
        violations.append(
            Violation(
                "incomplete-approval-decision-ledger",
                relative,
                f"record_approval_decision is missing {', '.join(missing)}",
            )
        )
    return violations


def _check_unused_tokio_dependencies(repo_root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for relative in ("crates/cli/Cargo.toml", "crates/app-server/Cargo.toml"):
        manifest_path = repo_root / relative
        manifest = _load_toml(manifest_path)
        dependencies = set((manifest.get("dependencies") or {}).keys())
        if "tokio" not in dependencies:
            continue
        crate_root = manifest_path.parent / "src"
        has_usage = any(
            "tokio::" in source or "#[tokio::" in source
            for source in _rust_sources(crate_root)
        )
        if not has_usage:
            violations.append(
                Violation(
                    "unused-tokio-dependency",
                    relative,
                    "tokio dependency is declared without tokio source usage",
                )
            )
    return violations


def _rust_sources(root: Path) -> list[str]:
    return [path.read_text(encoding="utf-8") for path in sorted(root.rglob("*.rs"))]


def _extract_rust_function_body(text: str, function_name: str) -> str | None:
    needles = (f"pub fn {function_name}", f"fn {function_name}")
    start = -1
    for needle in needles:
        start = text.find(needle)
        if start != -1:
            break
    if start == -1:
        return None
    opening = text.find("{", start)
    if opening == -1:
        return None
    depth = 0
    for index in range(opening, len(text)):
        char = text[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return text[opening + 1 : index]
    return None


def _extract_sql_table_bodies(text: str, table_name: str) -> list[str]:
    bodies: list[str] = []
    needle = "create table"
    search_from = 0
    lowered = text.lower()
    while True:
        start = lowered.find(needle, search_from)
        if start == -1:
            return bodies
        table_start = lowered.find(table_name.lower(), start)
        open_paren = text.find("(", start)
        if table_start == -1 or open_paren == -1 or table_start > open_paren:
            search_from = start + len(needle)
            continue
        depth = 0
        for index in range(open_paren, len(text)):
            char = text[index]
            if char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    bodies.append(text[open_paren + 1 : index])
                    search_from = index + 1
                    break
        else:
            return bodies


def _is_allowed_python_migration_path(path: str) -> bool:
    return any(path == allowed or path.startswith(f"{allowed}/") for allowed in ALLOWED_PYTHON_MIGRATION_PATHS)


def _git_changed_files(repo_root: Path) -> list[str]:
    commands = [
        ["git", "diff", "--name-only", "HEAD"],
        ["git", "diff", "--name-only", "--cached"],
        ["git", "ls-files", "--others", "--exclude-standard"],
    ]
    changed: list[str] = []
    for command in commands:
        completed = subprocess.run(command, cwd=repo_root, text=True, capture_output=True, check=False)
        if completed.returncode != 0:
            continue
        changed.extend(line.strip().replace("\\", "/") for line in completed.stdout.splitlines() if line.strip())
    if changed:
        return sorted(dict.fromkeys(changed))

    completed = subprocess.run(
        ["git", "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        cwd=repo_root,
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode == 0:
        changed.extend(line.strip().replace("\\", "/") for line in completed.stdout.splitlines() if line.strip())
    return sorted(dict.fromkeys(changed))


def _load_toml(path: Path) -> dict:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def _relative(path: Path, repo_root: Path) -> str:
    return path.resolve().relative_to(repo_root).as_posix()


if __name__ == "__main__":
    raise SystemExit(main())
