from __future__ import annotations

import threading
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from singularity.agent_host.models import RunEvent, RunStateSnapshot
from singularity.agent_host.sidecar import SidecarServer, _SidecarRun


def test_sidecar_health_and_unknown_method_are_json_rpc_shaped(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_FakeHost())

    health = server.handle({"id": 1, "method": "agent/health", "params": {}})
    unknown = server.handle({"id": 2, "method": "agent/unknown", "params": {}})

    assert health["result"]["status"] == "ok"
    assert health["result"]["component"] == "python_sidecar"
    assert unknown["error"]["code"] == -32601
    assert unknown["error"]["message"] == "Method not found: agent/unknown"


def test_sidecar_run_status_and_cancel_project_safe_payload(tmp_path: Path) -> None:
    host = _FakeHost()
    server = SidecarServer(tmp_path, host=host)

    run = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "finish task"}})
    status = server.handle({"id": 2, "method": "agent/status", "params": {"runId": "run_1"}})
    cancel = server.handle({"id": 3, "method": "agent/cancel", "params": {"runId": "run_1"}})

    result = run["result"]
    assert result["run_id"] == "run_1"
    assert result["session_id"] == "session_1"
    assert result["task_id"] == "task_1"
    assert result["status"] == "completed"
    assert result["trace_path"] == "run_1"
    assert result["events"] == [
        {
            "event_id": "event_1",
            "event_type": "lifecycle.run.started",
            "summary": "started",
            "component": "kernel",
            "severity": "info",
            "sequence": 0,
        }
    ]
    assert "raw prompt" not in str(result).lower()
    assert status["result"]["status"] == "completed"
    assert cancel["result"]["status"] == "cancelled"


def test_sidecar_cancel_unknown_run_returns_safe_not_found(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_FakeHost())

    result = server.handle({"id": 1, "method": "agent/cancel", "params": {"runId": "missing"}})

    assert "error" not in result
    assert result["result"] == {"run_id": "missing", "status": "not_found"}


def test_sidecar_cancel_and_fallback_status_return_safe_snapshot(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_SnapshotOnlyHost())

    cancel = server.handle({"id": 1, "method": "agent/cancel", "params": {"runId": "run_snapshot"}})
    status = server.handle({"id": 2, "method": "agent/status", "params": {"runId": "run_snapshot"}})

    expected = {
        "run_id": "run_snapshot",
        "session_id": "session_snapshot",
        "task_id": "task_snapshot",
        "status": "cancel_requested",
    }
    assert cancel["result"] == expected
    assert status["result"] == expected
    payload = f"{cancel} {status}".lower()
    for marker in ["final_answer", "final_report", "trace_run_dir", "event_count", "raw prompt", "provider_response"]:
        assert marker not in payload


def test_sidecar_redacts_secret_like_final_answer_and_event_summary(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_FakeHost(final_answer="token=sk-secret", summary="Authorization: Bearer secret123"))

    result = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "finish task"}})["result"]

    assert "sk-secret" not in result["final_answer"]
    assert "secret123" not in result["events"][0]["summary"]


def test_sidecar_rejects_invalid_json_and_missing_goal(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_FakeHost())

    invalid_json = server.handle_line("{")
    missing_goal = server.handle({"id": 2, "method": "agent/run", "params": {}})

    assert invalid_json["error"]["code"] == -32600
    assert "invalid JSON" in invalid_json["error"]["message"]
    assert missing_goal["error"]["code"] == -32603
    assert "goal is required" in missing_goal["error"]["message"]


def test_sidecar_resume_uses_agent_host_resume_run_and_model(tmp_path: Path) -> None:
    host = _FakeHost()
    server = SidecarServer(tmp_path, host=host)

    result = server.handle(
        {
            "id": 1,
            "method": "agent/resume",
            "params": {
                "sessionId": "session_previous",
                "goal": "continue task",
                "model": "gpt-test",
            },
        }
    )["result"]

    assert result["session_id"] == "session_previous"
    assert host.resume_calls == [("session_previous", "continue task", "gpt-test")]


def test_sidecar_run_returns_running_and_cancel_updates_active_run(tmp_path: Path) -> None:
    host = _SlowHost()
    server = SidecarServer(tmp_path, host=host)

    run = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "wait"}})
    status = server.handle({"id": 2, "method": "agent/status", "params": {"runId": "run_slow"}})
    cancel = server.handle({"id": 3, "method": "agent/cancel", "params": {"runId": "run_slow"}})

    assert run["result"]["status"] == "running"
    assert status["result"]["status"] == "running"
    assert cancel["result"]["status"] == "cancel_requested"
    assert host.cancelled


def test_sidecar_cancelled_background_run_reaches_terminal_status(tmp_path: Path) -> None:
    host = _CancellingHost()
    server = SidecarServer(tmp_path, host=host)

    run = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "wait"}})
    cancel = server.handle({"id": 2, "method": "agent/cancel", "params": {"runId": "run_cancel"}})
    deadline = time.monotonic() + 2.0
    status = None
    while time.monotonic() < deadline:
        status = server.handle({"id": 3, "method": "agent/status", "params": {"runId": "run_cancel"}})
        if status["result"]["status"] == "cancelled":
            break
        time.sleep(0.01)

    assert run["result"]["status"] == "running"
    assert cancel["result"]["status"] == "cancel_requested"
    assert status is not None
    assert status["result"]["status"] == "cancelled"


def test_sidecar_pending_registration_does_not_overwrite_terminal_result(tmp_path: Path) -> None:
    host = _SnapshotThenFinishHost()
    server = SidecarServer(tmp_path, host=host)
    host.server = server

    try:
        run = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "finish after snapshot"}})
        status = server.handle({"id": 2, "method": "agent/status", "params": {"runId": "run_race"}})
    finally:
        host.allow_return.set()

    assert run["result"]["status"] == "completed"
    assert status["result"]["status"] == "completed"
    assert status["result"]["final_answer"] == "done after snapshot"


def test_sidecar_slow_boot_returns_error_instead_of_pending_ids(tmp_path: Path) -> None:
    server = SidecarServer(tmp_path, host=_SlowBootHost())

    result = server.handle({"id": 1, "method": "agent/run", "params": {"goal": "wait"}})

    assert result["error"]["code"] == -32603
    assert "run id" in result["error"]["message"]
    assert "run_pending" not in result["error"]["message"]


class _FakeHost:
    def __init__(self, *, final_answer: str = "done", summary: str = "started") -> None:
        self.final_answer = final_answer
        self.summary = summary
        self.resume_calls: list[tuple[str, str, str | None]] = []

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        return self._run_result("session_1")

    def resume_run(self, session_id: str, goal: str, *, config: Any) -> SimpleNamespace:
        self.resume_calls.append((session_id, goal, config.model))
        return self._run_result(session_id)

    def _run_result(self, session_id: str) -> SimpleNamespace:
        return SimpleNamespace(
            run_id="run_1",
            session_id=session_id,
            task_id="task_1",
            status="completed",
            final_answer=self.final_answer,
            to_dict=lambda: {
                "run_id": "run_1",
                "session_id": session_id,
                "task_id": "task_1",
                "status": "completed",
                "final_answer": self.final_answer,
                "snapshot": self.snapshot("run_1").to_dict(),
            },
        )

    def snapshot(self, _run_id: str) -> RunStateSnapshot:
        return RunStateSnapshot(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            status="completed",
            trace_run_dir="work/traces/runs/run_1",
            event_count=1,
            artifact_count=0,
            last_sequence=0,
        )

    def events(self, _run_id: str) -> list[RunEvent]:
        return [
            RunEvent(
                event_id="event_1",
                event_type="lifecycle.run.started",
                run_id="run_1",
                component="kernel",
                severity="info",
                timestamp="2026-01-01T00:00:00+00:00",
                sequence=0,
                summary=self.summary,
                session_id="session_1",
                task_id="task_1",
                payload={"raw_prompt": "raw prompt"},
            )
        ]

    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        if _run_id != "run_1":
            from singularity.agent_host.host import AgentHostError

            raise AgentHostError(f"Unknown active run: {_run_id}")
        return RunStateSnapshot(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            status="cancelled",
            trace_run_dir="work/traces/runs/run_1",
            event_count=1,
            artifact_count=0,
            last_sequence=0,
        )


class _SnapshotOnlyHost:
    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        return self.snapshot(_run_id)

    def snapshot(self, _run_id: str) -> RunStateSnapshot:
        return RunStateSnapshot(
            run_id="run_snapshot",
            session_id="session_snapshot",
            task_id="task_snapshot",
            status="cancel_requested",
            trace_run_dir="work/traces/runs/run_snapshot",
            event_count=9,
            artifact_count=2,
            last_sequence=8,
            final_answer="raw prompt secret",
            final_report={"provider_response": "raw"},
        )


class _SlowHost:
    def __init__(self) -> None:
        self.cancelled = False
        self._sessions = {"run_slow": SimpleNamespace()}

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        while not self.cancelled:
            time.sleep(0.01)
        return SimpleNamespace(
            run_id="run_slow",
            session_id="session_slow",
            task_id="task_slow",
            status="cancelled",
            final_answer="",
            to_dict=lambda: {
                "run_id": "run_slow",
                "session_id": "session_slow",
                "task_id": "task_slow",
                "status": "cancelled",
                "final_answer": "",
                "snapshot": self.snapshot("run_slow").to_dict(),
            },
        )

    def resume_run(self, _session_id: str, goal: str, *, config: Any) -> SimpleNamespace:
        return self.start_run(goal, config=config)

    def snapshot(self, run_id: str) -> RunStateSnapshot:
        return RunStateSnapshot(
            run_id=run_id,
            session_id="session_slow",
            task_id="task_slow",
            status="cancel_requested" if self.cancelled else "running",
            trace_run_dir="work/traces/runs/run_slow",
            event_count=0,
            artifact_count=0,
            last_sequence=None,
        )

    def events(self, _run_id: str) -> list[RunEvent]:
        return []

    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        self.cancelled = True
        return self.snapshot("run_slow")


class _CancellingHost(_SlowHost):
    def __init__(self) -> None:
        super().__init__()
        self._sessions = {"run_cancel": SimpleNamespace()}

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        while not self.cancelled:
            time.sleep(0.01)
        from singularity.kernel import CancellationError

        raise CancellationError("cancelled", code="cancelled")

    def snapshot(self, run_id: str) -> RunStateSnapshot:
        return RunStateSnapshot(
            run_id=run_id,
            session_id="session_cancel",
            task_id="task_cancel",
            status="cancel_requested" if self.cancelled else "running",
            trace_run_dir="work/traces/runs/run_cancel",
            event_count=0,
            artifact_count=0,
            last_sequence=None,
        )

    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        self.cancelled = True
        return self.snapshot("run_cancel")


class _SnapshotThenFinishHost:
    def __init__(self) -> None:
        self._sessions = {"run_race": SimpleNamespace()}
        self.server: SidecarServer | None = None
        self.release_run = threading.Event()
        self.terminal_written = threading.Event()
        self.allow_return = threading.Event()
        self.released_from_snapshot = False

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        assert self.release_run.wait(1.0)
        result = self._result()
        assert self.server is not None
        with self.server._lock:
            self.server._runs["run_race"] = _SidecarRun(
                run_id="run_race",
                session_id="session_race",
                task_id="task_race",
                status="completed",
                result=result.to_dict(),
            )
        self.terminal_written.set()
        assert self.allow_return.wait(1.0)
        return result

    def _result(self) -> SimpleNamespace:
        return SimpleNamespace(
            run_id="run_race",
            session_id="session_race",
            task_id="task_race",
            status="completed",
            final_answer="done after snapshot",
            to_dict=lambda: {
                "run_id": "run_race",
                "session_id": "session_race",
                "task_id": "task_race",
                "status": "completed",
                "final_answer": "done after snapshot",
                "snapshot": {
                    "run_id": "run_race",
                    "session_id": "session_race",
                    "task_id": "task_race",
                    "status": "completed",
                    "trace_run_dir": "work/traces/runs/run_race",
                    "event_count": 0,
                    "artifact_count": 0,
                    "last_sequence": None,
                },
            },
        )

    def resume_run(self, _session_id: str, goal: str, *, config: Any) -> SimpleNamespace:
        return self.start_run(goal, config=config)

    def snapshot(self, run_id: str) -> RunStateSnapshot:
        if not self.released_from_snapshot and self.server is not None:
            self.released_from_snapshot = True
            self.release_run.set()
            assert self.terminal_written.wait(1.0)
        return RunStateSnapshot(
            run_id=run_id,
            session_id="session_race",
            task_id="task_race",
            status="running",
            trace_run_dir="work/traces/runs/run_race",
            event_count=0,
            artifact_count=0,
            last_sequence=None,
        )

    def events(self, _run_id: str) -> list[RunEvent]:
        return []

    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        return self.snapshot("run_race")


class _SlowBootHost:
    def __init__(self) -> None:
        self._sessions: dict[str, SimpleNamespace] = {}

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        time.sleep(3)
        return SimpleNamespace(
            run_id="run_late",
            session_id="session_late",
            task_id="task_late",
            status="completed",
            final_answer="",
            to_dict=lambda: {
                "run_id": "run_late",
                "session_id": "session_late",
                "task_id": "task_late",
                "status": "completed",
                "final_answer": "",
                "snapshot": self.snapshot("run_late").to_dict(),
            },
        )

    def resume_run(self, _session_id: str, goal: str, *, config: Any) -> SimpleNamespace:
        return self.start_run(goal, config=config)

    def snapshot(self, run_id: str) -> RunStateSnapshot:
        return RunStateSnapshot(
            run_id=run_id,
            session_id="session_late",
            task_id="task_late",
            status="running",
            trace_run_dir="work/traces/runs/run_late",
            event_count=0,
            artifact_count=0,
            last_sequence=None,
        )

    def events(self, _run_id: str) -> list[RunEvent]:
        return []

    def cancel_run(self, _run_id: str) -> RunStateSnapshot:
        return self.snapshot("run_late")
