#!/usr/bin/env python3
"""Smoke-test the Rust CLI -> app-server -> Python sidecar route."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
APP_SERVER_BINARY = REPO_ROOT / "target" / "debug" / f"singularity_app_server{'.exe' if os.name == 'nt' else ''}"
SAFE_ENV_ALLOWLIST = {
    "CARGO_HOME",
    "HOME",
    "PATH",
    "PATHEXT",
    "RUSTUP_HOME",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "WINDIR",
}
SECRET_ENV_MARKERS = (
    "API_KEY",
    "AUTH",
    "CREDENTIAL",
    "PASSWORD",
    "SECRET",
    "TOKEN",
)


def main() -> int:
    build = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "singularity_app_server",
            "--bin",
            "singularity_app_server",
        ],
        cwd=REPO_ROOT,
        text=True,
        check=False,
    )
    if build.returncode != 0:
        return build.returncode

    with tempfile.TemporaryDirectory(prefix="rust-cli-agent-host-") as tmp:
        db_path = Path(tmp) / "sessions.sqlite3"
        env = _safe_smoke_env()
        env["SINGULARITY_APP_SERVER_BIN"] = str(APP_SERVER_BINARY)
        env["SINGULARITY_APP_SERVER_DB"] = str(db_path)
        env["SINGULARITY_SIDECAR_TEST_MODE"] = "completed"
        env["PYTHONPATH"] = _prepend_path(REPO_ROOT / "src", env.get("PYTHONPATH"))
        python_bin = _workspace_python()
        if python_bin is not None:
            env["SINGULARITY_PYTHON_SIDECAR_BIN"] = str(python_bin)

        command = [
            "cargo",
            "run",
            "-p",
            "singularity_cli",
            "--bin",
            "sg",
            "--",
            "run",
            "verify Rust CLI Python sidecar route",
            "--agent-host",
            "python",
        ]
        completed = subprocess.run(
            command,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode != 0:
            sys.stderr.write(completed.stderr)
            sys.stderr.write(completed.stdout)
            return completed.returncode
        stdout = completed.stdout
        required = [
            "thread ",
            "turn ",
            "agent_loop_status=completed",
            "assistant sidecar completed",
        ]
        missing = [marker for marker in required if marker not in stdout]
        if missing:
            sys.stderr.write(f"missing CLI output markers: {missing}\n{stdout}\n")
            return 1
        thread_id = _first_prefixed_value(stdout, "thread ")
        if thread_id is None:
            sys.stderr.write(f"could not parse thread id\n{stdout}\n")
            return 1

        trace = subprocess.run(
            [
                "cargo",
                "run",
                "-p",
                "singularity_cli",
                "--bin",
                "sg",
                "--",
                "trace",
                thread_id,
                "--limit",
                "20",
            ],
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        if trace.returncode != 0:
            sys.stderr.write(trace.stderr)
            sys.stderr.write(trace.stdout)
            return trace.returncode
        if "python_sidecar" not in trace.stdout:
            sys.stderr.write(f"trace did not include python_sidecar\n{trace.stdout}\n")
            return 1

    print("rust CLI agent host smoke verified")
    return 0


def _workspace_python() -> Path | None:
    candidates = [
        REPO_ROOT / ".venv" / "Scripts" / "python.exe",
        REPO_ROOT / ".venv" / "bin" / "python",
    ]
    return next((path for path in candidates if path.exists()), None)


def _safe_smoke_env() -> dict[str, str]:
    env = {
        name: value
        for name, value in os.environ.items()
        if name.upper() in SAFE_ENV_ALLOWLIST and not _is_secret_env_name(name)
    }
    for name in list(os.environ):
        if (name.startswith("CARGO_") or name.startswith("RUST")) and not _is_secret_env_name(
            name
        ):
            env[name] = os.environ[name]
    return env


def _is_secret_env_name(name: str) -> bool:
    upper_name = name.upper()
    return any(marker in upper_name for marker in SECRET_ENV_MARKERS)


def _prepend_path(path: Path, existing: str | None) -> str:
    if existing:
        return f"{path}{os.pathsep}{existing}"
    return str(path)


def _first_prefixed_value(text: str, prefix: str) -> str | None:
    for line in text.splitlines():
        if line.startswith(prefix):
            return line[len(prefix) :].strip()
    return None


if __name__ == "__main__":
    raise SystemExit(main())
