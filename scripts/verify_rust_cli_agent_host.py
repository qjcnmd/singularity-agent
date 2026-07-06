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
        fake_app_server = _write_fake_lifecycle_app_server(Path(tmp))
        fake_db_path = Path(tmp) / "fake-lifecycle.sqlite3"
        fake_env = _safe_smoke_env()
        fake_env["SINGULARITY_APP_SERVER_BIN"] = str(fake_app_server)
        fake_env["SINGULARITY_APP_SERVER_DB"] = str(fake_db_path)
        fake_env["PYTHONPATH"] = _prepend_path(REPO_ROOT / "src", fake_env.get("PYTHONPATH"))
        for args, required in (
            (
                ["run", "verify lifecycle", "--agent-host", "python"],
                ("thread thread_fake", "turn turn_fake running agent_loop_status=running"),
            ),
            (
                ["continue", "thread_fake", "resume lifecycle", "--agent-host", "python"],
                ("thread thread_fake", "turn turn_continue completed agent_loop_status=completed"),
            ),
            (
                ["turn", "status", "turn_fake"],
                ("turn turn_fake running agent_loop_status=running",),
            ),
            (
                ["turn", "interrupt", "turn_fake"],
                ("turn turn_fake interrupted agent_loop_status=cancel_requested",),
            ),
            (
                ["trace", "thread_fake", "--limit", "20"],
                ("turn lifecycle cancel_requested", "python_sidecar"),
            ),
        ):
            smoke = _run_sg(args, fake_env)
            if smoke.returncode != 0:
                sys.stderr.write(smoke.stderr)
                sys.stderr.write(smoke.stdout)
                return smoke.returncode
            missing = [marker for marker in required if marker not in smoke.stdout]
            if missing:
                sys.stderr.write(
                    f"missing lifecycle smoke markers for {args}: {missing}\n{smoke.stdout}\n"
                )
                return 1

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
            "run",
            "verify Rust CLI Python sidecar route",
            "--agent-host",
            "python",
        ]
        completed = _run_sg(command, env)
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

        trace = _run_sg(["trace", thread_id, "--limit", "20"], env)
        if trace.returncode != 0:
            sys.stderr.write(trace.stderr)
            sys.stderr.write(trace.stdout)
            return trace.returncode
        if "python_sidecar" not in trace.stdout:
            sys.stderr.write(f"trace did not include python_sidecar\n{trace.stdout}\n")
            return 1

    print("rust CLI agent host smoke verified")
    return 0


def _run_sg(args: list[str], env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "cargo",
            "run",
            "-p",
            "singularity_cli",
            "--bin",
            "sg",
            "--",
            *args,
        ],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
    )


def _write_fake_lifecycle_app_server(directory: Path) -> Path:
    script_path = directory / "fake_lifecycle_app_server.py"
    script_path.write_text(
        r'''
import json
import sys

status_calls = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        print(json.dumps({"id": request_id, "result": {"userAgent": "fake", "platformFamily": "local", "platformOs": "test"}}), flush=True)
    elif method == "initialized":
        continue
    elif method == "thread/start":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": "thread_fake", "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "thread/read":
        print(json.dumps({"id": request_id, "result": {"thread": {"thread_id": message["params"]["threadId"], "model": None, "cwd": None, "status": "active"}}}), flush=True)
    elif method == "turn/start":
        turn_id = "turn_continue" if message["params"].get("threadId") == "thread_fake" and "resume lifecycle" in str(message["params"].get("input")) else "turn_fake"
        status = "completed" if turn_id == "turn_continue" else "running"
        print(json.dumps({"method": "turn/started", "params": {"turn": {"turn_id": turn_id, "thread_id": "thread_fake", "status": "running", "agent_loop_status": "running"}}}), flush=True)
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": turn_id, "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
    elif method == "turn/status":
        status_calls += 1
        status = "completed" if status_calls >= 2 else "running"
        print(json.dumps({"id": request_id, "result": {"turn": {"turn_id": message["params"]["turnId"], "thread_id": "thread_fake", "status": status, "agent_loop_status": status}}}), flush=True)
    elif method == "turn/interrupt":
        print(json.dumps({"id": request_id, "result": {"turnId": message["params"]["turnId"], "status": "interrupted", "agent_loop_status": "cancel_requested"}}), flush=True)
    elif method == "trace/tail":
        print(json.dumps({"id": request_id, "result": {"events": [
            {"event_id": "trace_cancel", "component": "app_server", "summary": "turn lifecycle cancel_requested"},
            {"event_id": "trace_sidecar", "component": "python_sidecar", "summary": "Python sidecar result translated"}
        ]}}), flush=True)
''',
        encoding="utf-8",
    )
    if os.name == "nt":
        launcher = directory / "fake_lifecycle_app_server.cmd"
        launcher.write_text(
            f'@echo off\r\npython "{script_path}"\r\nexit /b %ERRORLEVEL%\r\n',
            encoding="utf-8",
        )
        return launcher
    launcher = directory / "fake_lifecycle_app_server"
    launcher.write_text(f"#!/bin/sh\nexec python3 '{script_path}'\n", encoding="utf-8")
    launcher.chmod(0o755)
    return launcher


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
