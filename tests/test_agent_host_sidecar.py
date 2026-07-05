from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any

from singularity.agent_host.models import RunEvent, RunStateSnapshot
from singularity.agent_host.sidecar import SidecarServer


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


class _FakeHost:
    def __init__(self, *, final_answer: str = "done", summary: str = "started") -> None:
        self.final_answer = final_answer
        self.summary = summary

    def start_run(self, _goal: str, *, config: Any) -> SimpleNamespace:
        assert config.interaction_mode.value == "non_interactive"
        return SimpleNamespace(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            status="completed",
            final_answer=self.final_answer,
            to_dict=lambda: {
                "run_id": "run_1",
                "session_id": "session_1",
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
