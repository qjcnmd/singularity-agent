#!/usr/bin/env python3
"""Guard CLI-first Rust Agent Host migration boundaries."""

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
        "dependencies": {"schemars", "serde", "serde_json", "singularity_core"},
        "dev-dependencies": set(),
    },
    "crates/model/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json"},
        "dev-dependencies": set(),
    },
    "crates/agent/Cargo.toml": {
        "dependencies": {"schemars", "serde", "serde_json"},
        "dev-dependencies": set(),
    },
    "crates/app-server/Cargo.toml": {
        "dependencies": {
            "schemars",
            "serde",
            "serde_json",
            "singularity_agent",
            "singularity_core",
            "singularity_policy",
            "singularity_protocol",
            "singularity_store",
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
    "scripts/export_rust_parity_fixtures.py",
    "tests/test_rust_parity_fixtures.py",
    "tests/test_rust_migration_boundaries.py",
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

TOOL_OBSERVATION_MODEL_PAYLOAD_FORBIDDEN = {
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
}

RUST_AGENT_HOST_DOC_MARKERS = {
    "Current Python owner",
    "Rust owner after this stage",
    "Parity expectation",
    "Intentional divergence",
    "AgentLoopStatusBridge",
    "SessionStore.create_turn_with_input_and_trace",
}

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
    violations.extend(_check_cli_sidecar_surface(repo_root))
    violations.extend(_check_sidecar_resume_and_model(repo_root))
    violations.extend(_check_no_fake_agent_delta(repo_root))
    violations.extend(_check_rust_cli_smoke_env(repo_root))
    violations.extend(_check_sidecar_trace_projection(repo_root))
    violations.extend(_check_tool_observation_payload(repo_root))
    violations.extend(_check_sandbox_phase1_boundary(repo_root))
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
                "Python runtime changes must stay in sidecar/oracle/fixture/parity allowlist during Rust migration",
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
    if "AgentLoopStatusBridge::not_migrated()" not in text:
        return [
            Violation(
                "agent-loop-status-drift",
                _relative(app_server, repo_root),
                'turn/start must keep explicit not_migrated status until the Rust AgentLoop milestone',
            )
        ]
    agent_text = agent.read_text(encoding="utf-8")
    if "pub struct NativeAgentLoopCapability" not in agent_text or "available: false" not in agent_text:
        return [
            Violation(
                "native-agent-loop-capability-drift",
                _relative(agent, repo_root),
                "NativeAgentLoopCapability must stay explicitly unavailable until Rust AgentLoop is migrated",
            )
        ]
    if "status: AgentHostStatus::NotMigrated" not in agent_text:
        return [
            Violation(
                "native-agent-loop-status-drift",
                _relative(agent, repo_root),
                "NativeAgentLoopCapability must report NotMigrated until full Rust AgentLoop is implemented",
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


def _check_rust_agent_host_docs(repo_root: Path) -> list[Violation]:
    docs = [
        repo_root / "docs" / "singularity.md",
        repo_root / "docs" / "architecture" / "modules" / "rust-app-server-protocol.md",
    ]
    text = "\n".join(path.read_text(encoding="utf-8") for path in docs)
    return [
        Violation(
            "rust-agent-host-docs-incomplete",
            "docs/singularity.md,docs/architecture/modules/rust-app-server-protocol.md",
            f"Rust CLI-first migration docs must include marker: {marker}",
        )
        for marker in sorted(RUST_AGENT_HOST_DOC_MARKERS)
        if marker not in text
    ]


def _check_cli_sidecar_surface(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "cli" / "src" / "main.rs"
    text = path.read_text(encoding="utf-8")
    if "AgentHost::Python" in text and "SINGULARITY_PYTHON_SIDECAR" in text:
        return []
    return [
        Violation(
            "raw-sidecar-env-user-setup",
            _relative(path, repo_root),
            "CLI must expose a user-facing Python agent-host option instead of requiring raw SINGULARITY_PYTHON_SIDECAR setup",
        )
    ]


def _check_sidecar_resume_and_model(repo_root: Path) -> list[Violation]:
    agent_path = repo_root / "crates" / "agent" / "src" / "lib.rs"
    app_server_path = repo_root / "crates" / "app-server" / "src" / "lib.rs"
    sidecar_path = repo_root / "src" / "singularity" / "agent_host" / "sidecar.py"
    agent_text = agent_path.read_text(encoding="utf-8")
    app_server_text = app_server_path.read_text(encoding="utf-8")
    sidecar_text = sidecar_path.read_text(encoding="utf-8") if sidecar_path.exists() else ""
    checks = [
        (
            agent_path,
            agent_text,
            "SIDECAR_METHOD_RESUME",
            "sidecar-resume-missing",
            "Rust sidecar client must expose agent/resume for sg continue --agent-host python",
        ),
        (
            agent_path,
            agent_text,
            "sidecar_run_params(goal, model)",
            "sidecar-model-forwarding-missing",
            "Rust sidecar client must forward thread model to Python sidecar",
        ),
        (
            app_server_path,
            app_server_text,
            "previous_python_session_id",
            "sidecar-resume-session-missing",
            "app-server must derive safe previous Python session_id for sidecar resume",
        ),
        (
            app_server_path,
            app_server_text,
            "client.resume_agent(session_id, &goal, model)",
            "sidecar-resume-call-missing",
            "app-server continue path must call Python sidecar agent/resume",
        ),
        (
            sidecar_path,
            sidecar_text,
            "METHOD_RESUME",
            "python-sidecar-resume-method-missing",
            "Python sidecar must accept agent/resume",
        ),
        (
            sidecar_path,
            sidecar_text,
            "model=_optional_str(params.get(\"model\"))",
            "python-sidecar-model-forwarding-missing",
            "Python sidecar must pass model into ProductionConfig",
        ),
    ]
    return [
        Violation(code, _relative(path, repo_root), detail)
        for path, text, marker, code, detail in checks
        if marker not in text
    ]


def _check_no_fake_agent_delta(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "app-server" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    if "input accepted" not in text:
        return []
    return [
        Violation(
            "fake-agent-delta",
            _relative(path, repo_root),
            "no-sidecar turn/start must not emit a fake assistant delta such as input accepted",
        )
    ]


def _check_rust_cli_smoke_env(repo_root: Path) -> list[Violation]:
    path = repo_root / "scripts" / "verify_rust_cli_agent_host.py"
    if not path.exists():
        return [
            Violation(
                "rust-cli-agent-host-smoke-missing",
                _relative(path, repo_root),
                "Rust CLI agent host smoke script is required",
            )
        ]
    text = path.read_text(encoding="utf-8")
    violations: list[Violation] = []
    if "os.environ.copy()" in text:
        violations.append(
            Violation(
                "rust-cli-smoke-env-copy",
                _relative(path, repo_root),
                "sidecar smoke must not copy the full process environment with provider secrets",
            )
        )
    for marker in ("SECRET_ENV_MARKERS", "SAFE_ENV_ALLOWLIST", "_safe_smoke_env"):
        if marker not in text:
            violations.append(
                Violation(
                    "rust-cli-smoke-env-scrub-missing",
                    _relative(path, repo_root),
                    f"sidecar smoke must define {marker} to scrub provider secrets",
                )
            )
    return violations


def _check_sidecar_trace_projection(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "agent" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    body = _extract_rust_function_body(text, "sidecar_trace_summary")
    if body is None:
        return [
            Violation(
                "sidecar-trace-summary-missing",
                _relative(path, repo_root),
                "sidecar_trace_summary not found",
            )
        ]
    lowered = body.lower()
    return [
        Violation(
            "sidecar-trace-projection-leak",
            _relative(path, repo_root),
            f"sidecar_trace_summary references forbidden marker {marker}",
        )
        for marker in sorted(SIDECAR_TRACE_PROJECTION_FORBIDDEN)
        if marker in lowered
    ]


def _check_tool_observation_payload(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "tools" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    body = _extract_rust_function_body(text, "to_model_payload")
    violations: list[Violation] = []
    if body is None:
        return [Violation("tool-observation-payload-missing", _relative(path, repo_root), "to_model_payload not found")]
    lowered = body.lower()
    for marker in sorted(TOOL_OBSERVATION_MODEL_PAYLOAD_FORBIDDEN):
        if marker in lowered:
            violations.append(
                Violation(
                    "tool-observation-model-leak",
                    _relative(path, repo_root),
                    f"to_model_payload references model-forbidden marker {marker}",
                )
            )
    for field in ("policy_decision_id", "approval_grant_id", "internal_metadata"):
        if f"#[serde(skip)]\n    {field}" not in text:
            violations.append(
                Violation(
                    "tool-observation-internal-field-serialized",
                    _relative(path, repo_root),
                    f"{field} must stay serde-skipped",
                )
            )
    return violations


def _check_sandbox_phase1_boundary(repo_root: Path) -> list[Violation]:
    path = repo_root / "crates" / "sandbox" / "src" / "lib.rs"
    text = path.read_text(encoding="utf-8")
    relative = _relative(path, repo_root)
    checks = (
        ("relaxed-sandbox-filesystem-mode", "HostWorkspace", "SandboxFilesystemMode must not expose host-workspace/no-sandbox mode in Phase 1"),
        ("relaxed-sandbox-backend-enforcement", "Relaxed", "SandboxBackendEnforcement must not expose relaxed backend mode in Phase 1"),
        ("relaxed-sandbox-executor", "pub struct CommandExecutor", "sandbox crate must not expose a local process executor in Phase 1"),
        (
            "relaxed-sandbox-command-request",
            "pub fn local_process",
            "CommandRequest must not expose a relaxed host local-process constructor",
        ),
        ("relaxed-sandbox-run-local", "pub fn run_local", "sandbox crate must not expose run_local without a strict backend"),
        ("sandbox-host-patch-executor", "pub struct PatchExecutor", "sandbox crate must not expose host filesystem mutation executor in Phase 1"),
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
                "sandbox crate must not spawn host processes until a strict backend is implemented",
            )
        )
    if any(marker in text for marker in ("fs::write", "std::fs::write", "fs::remove_file", "std::fs::remove_file")):
        violations.append(
            Violation(
                "direct-sandbox-filesystem-mutation",
                relative,
                "sandbox crate must not mutate host filesystem paths in Phase 1",
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
    needle = f"pub fn {function_name}"
    start = text.find(needle)
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
