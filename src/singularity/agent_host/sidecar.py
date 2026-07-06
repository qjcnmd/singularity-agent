from __future__ import annotations

import json
import os
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from singularity.agent_host.host import AgentHost, AgentHostError
from singularity.config import ProductionConfig
from singularity.interaction import InteractionMode
from singularity.kernel import CancellationError
from singularity.observability import shared_trace_redactor

JSON_RPC_INVALID_REQUEST = -32600
JSON_RPC_METHOD_NOT_FOUND = -32601
JSON_RPC_INTERNAL_ERROR = -32603
JSON_RPC_NOT_FOUND = -32004

METHOD_RUN = "agent/run"
METHOD_RESUME = "agent/resume"
METHOD_CANCEL = "agent/cancel"
METHOD_STATUS = "agent/status"
METHOD_HEALTH = "agent/health"

DEFAULT_MAX_EVENTS = 20
RUN_ID_WAIT_SECONDS = 2.0
RUN_ID_POLL_SECONDS = 0.01
RUN_ID_DISCOVERY_TIMEOUT_MESSAGE = "sidecar run did not publish a run id before lifecycle timeout"
_REDACTOR = shared_trace_redactor()


def main() -> None:
    server = SidecarServer.from_env()
    for line in sys.stdin:
        text = line.strip()
        if not text:
            continue
        print(json.dumps(server.handle_line(text), ensure_ascii=False, sort_keys=True), flush=True)


class SidecarServer:
    def __init__(
        self,
        project_root: Path | str,
        *,
        host: AgentHost | None = None,
    ) -> None:
        self.project_root = Path(project_root).expanduser().resolve(strict=False)
        self.host = host or AgentHost(self.project_root)
        self._runs: dict[str, _SidecarRun] = {}
        self._lock = threading.Lock()

    @classmethod
    def from_env(cls) -> SidecarServer:
        project_root = Path(os.getenv("SINGULARITY_SIDECAR_PROJECT_ROOT") or Path.cwd())
        test_mode = os.getenv("SINGULARITY_SIDECAR_TEST_MODE")
        host = _SidecarTestHost(test_mode, project_root) if test_mode else None
        return cls(project_root, host=host)

    def handle_line(self, line: str) -> dict[str, Any]:
        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            return _error(None, JSON_RPC_INVALID_REQUEST, f"invalid JSON: {exc.msg}")
        return self.handle(message)

    def handle(self, message: dict[str, Any]) -> dict[str, Any]:
        request_id = message.get("id")
        method = message.get("method")
        params = message.get("params") or {}
        if not isinstance(method, str):
            return _error(request_id, JSON_RPC_INVALID_REQUEST, "Missing method")
        if not isinstance(params, dict):
            return _error(request_id, JSON_RPC_INVALID_REQUEST, "params must be an object")

        try:
            if method == METHOD_HEALTH:
                return _response(request_id, {"status": "ok", "component": "python_sidecar"})
            if method == METHOD_RUN:
                return _response(request_id, self._run(params))
            if method == METHOD_RESUME:
                return _response(request_id, self._resume(params))
            if method == METHOD_CANCEL:
                return _response(request_id, self._cancel(params))
            if method == METHOD_STATUS:
                return _response(request_id, self._status(params))
        except AgentHostError as exc:
            return _error(request_id, JSON_RPC_NOT_FOUND, _REDACTOR.redact_text(str(exc)))
        except Exception as exc:
            return _error(request_id, JSON_RPC_INTERNAL_ERROR, _REDACTOR.redact_text(str(exc)))

        return _error(request_id, JSON_RPC_METHOD_NOT_FOUND, f"Method not found: {method}")

    def _run(self, params: dict[str, Any]) -> dict[str, Any]:
        goal = str(params.get("goal") or "")
        if not goal.strip():
            raise ValueError("goal is required")
        result = self._start_background_run(
            lambda: self.host.start_run(goal, config=self._config(params))
        )
        if result.result is not None:
            return _safe_run_result(self.host, result.run_id, result.result)
        return _safe_snapshot(result.run_id, result.session_id, result.task_id, result.status)

    def _resume(self, params: dict[str, Any]) -> dict[str, Any]:
        session_id = str(params.get("sessionId") or params.get("session_id") or "")
        goal = str(params.get("goal") or "")
        if not session_id.strip():
            raise ValueError("sessionId is required")
        if not goal.strip():
            raise ValueError("goal is required")
        result = self._start_background_run(
            lambda: self.host.resume_run(session_id, goal, config=self._config(params))
        )
        if result.result is not None:
            return _safe_run_result(self.host, result.run_id, result.result)
        return _safe_snapshot(result.run_id, result.session_id, result.task_id, result.status)

    def _cancel(self, params: dict[str, Any]) -> dict[str, Any]:
        run_id = str(params.get("runId") or params.get("run_id") or "")
        if not run_id:
            raise ValueError("runId is required")
        try:
            snapshot = self.host.cancel_run(run_id)
            with self._lock:
                run = self._runs.get(run_id)
                if run is not None:
                    run.status = str(snapshot.status)
            return _safe_snapshot_from_host_snapshot(snapshot, fallback_run_id=run_id)
        except AgentHostError:
            return {"run_id": run_id, "status": "not_found"}

    def _status(self, params: dict[str, Any]) -> dict[str, Any]:
        run_id = str(params.get("runId") or params.get("run_id") or "")
        if not run_id:
            raise ValueError("runId is required")
        with self._lock:
            run = self._runs.get(run_id)
        if run is not None and run.result is not None:
            return _safe_run_result(self.host, run_id, run.result)
        if run is not None and run.error is not None:
            status = "cancelled" if isinstance(run.error, CancellationError) else "failed"
            snapshot = self.host.snapshot(run_id)
            session_id = str(snapshot.session_id or run.session_id)
            task_id = str(snapshot.task_id or run.task_id)
            with self._lock:
                current = self._runs.get(run_id)
                if current is not None:
                    current.status = status
            return _safe_snapshot(run_id, session_id, task_id, status)
        if run is not None:
            snapshot = self.host.snapshot(run_id)
            status = str(snapshot.status or run.status)
            session_id = str(snapshot.session_id or run.session_id)
            task_id = str(snapshot.task_id or run.task_id)
            with self._lock:
                current = self._runs.get(run_id)
                if current is not None:
                    current.status = status
            return _safe_snapshot(run_id, session_id, task_id, status)
        return _safe_snapshot_from_host_snapshot(
            self.host.snapshot(run_id),
            fallback_run_id=run_id,
        )

    def _config(self, params: dict[str, Any]) -> ProductionConfig:
        return ProductionConfig.from_cli(
            project_root=self.project_root,
            approval_policy=str(params.get("approvalPolicy") or "never"),
            interaction_mode=InteractionMode.NON_INTERACTIVE,
            dry_run=bool(params.get("dryRun", False)),
            max_turns=_optional_int(params.get("maxTurns")),
            model=_optional_str(params.get("model")),
            base_url=_optional_str(params.get("baseUrl")),
            trace_dir=_optional_path(params.get("traceDir")),
        )

    def _start_background_run(self, run: Any) -> _SidecarRun:
        holder: dict[str, Any] = {}
        ready = threading.Event()

        def target() -> None:
            try:
                result = run()
                with self._lock:
                    current = self._runs.get(result.run_id)
                    if current is None:
                        current = _SidecarRun(
                            run_id=result.run_id,
                            session_id=result.session_id,
                            task_id=result.task_id,
                            status="running",
                        )
                        self._runs[result.run_id] = current
                    current.status = str(result.status)
                    current.result = result.to_dict()
                    holder["run"] = current
            except Exception as exc:  # pragma: no cover - defensive thread boundary
                holder["error"] = exc
                snapshot = _latest_snapshot(self.host)
                if snapshot is not None:
                    status = "cancelled" if isinstance(exc, CancellationError) else "failed"
                    with self._lock:
                        current = self._runs.get(snapshot.run_id)
                        if current is None:
                            current = _SidecarRun(
                                run_id=snapshot.run_id,
                                session_id=str(snapshot.session_id or snapshot.run_id),
                                task_id=str(snapshot.task_id or snapshot.run_id),
                                status=status,
                            )
                            self._runs[snapshot.run_id] = current
                        current.status = status
                        current.error = exc
            finally:
                ready.set()

        thread = threading.Thread(target=target, daemon=True)
        thread.start()
        deadline = time.monotonic() + RUN_ID_WAIT_SECONDS
        snapshot = None
        while not ready.wait(RUN_ID_POLL_SECONDS):
            snapshot = _latest_snapshot(self.host)
            if snapshot is not None or time.monotonic() >= deadline:
                break
        if "error" in holder:
            raise holder["error"]
        if "run" in holder:
            return holder["run"]
        snapshot = snapshot or _latest_snapshot(self.host)
        if snapshot is None:
            raise TimeoutError(RUN_ID_DISCOVERY_TIMEOUT_MESSAGE)
        run_id = snapshot.run_id
        session_id = str(snapshot.session_id or run_id)
        task_id = str(snapshot.task_id or run_id)
        pending = _SidecarRun(
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            status="running",
            thread=thread,
        )
        with self._lock:
            existing = self._runs.get(run_id)
            if existing is not None:
                if existing.thread is None:
                    existing.thread = thread
                return existing
            self._runs[run_id] = pending
            return pending


def _safe_run_result(host: AgentHost, run_id: str, result: dict[str, Any]) -> dict[str, Any]:
    snapshot = dict(result.get("snapshot") or {})
    events = [_safe_event(event.to_dict()) for event in host.events(run_id)[-DEFAULT_MAX_EVENTS:]]
    return {
        "run_id": str(result.get("run_id") or run_id),
        "session_id": str(result.get("session_id") or snapshot.get("session_id") or run_id),
        "task_id": str(result.get("task_id") or snapshot.get("task_id") or run_id),
        "status": str(result.get("status") or snapshot.get("status") or "unknown"),
        "final_answer": _REDACTOR.redact_text(str(result.get("final_answer") or "")),
        "trace_path": _safe_trace_path(snapshot.get("trace_run_dir")),
        "events": events,
    }


def _safe_snapshot(run_id: str, session_id: str, task_id: str, status: str) -> dict[str, Any]:
    return {
        "run_id": run_id,
        "session_id": session_id,
        "task_id": task_id,
        "status": status,
    }


def _safe_snapshot_from_host_snapshot(snapshot: Any, *, fallback_run_id: str) -> dict[str, Any]:
    run_id = str(getattr(snapshot, "run_id", None) or fallback_run_id)
    return _safe_snapshot(
        run_id,
        str(getattr(snapshot, "session_id", None) or run_id),
        str(getattr(snapshot, "task_id", None) or run_id),
        str(getattr(snapshot, "status", None) or "unknown"),
    )


def _latest_snapshot(host: AgentHost) -> Any | None:
    sessions = getattr(host, "_sessions", None)
    if isinstance(sessions, dict) and sessions:
        run_id = next(reversed(sessions))
        return host.snapshot(run_id)
    return None


def _safe_event(event: dict[str, Any]) -> dict[str, Any]:
    return {
        "event_id": str(event.get("event_id") or ""),
        "event_type": str(event.get("event_type") or ""),
        "summary": _REDACTOR.redact_text(str(event.get("summary") or "")),
        "component": str(event.get("component") or ""),
        "severity": str(event.get("severity") or "info"),
        "sequence": int(event.get("sequence") or 0),
    }


def _safe_trace_path(value: Any) -> str | None:
    if value is None:
        return None
    path = Path(str(value))
    return path.name or None


def _response(request_id: Any, result: dict[str, Any]) -> dict[str, Any]:
    return {"id": request_id, "result": result}


def _error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"id": request_id, "error": {"code": code, "message": _REDACTOR.redact_text(message)}}


def _optional_int(value: Any) -> int | None:
    if value is None:
        return None
    return int(value)


def _optional_str(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text or None


def _optional_path(value: Any) -> Path | None:
    if value is None:
        return None
    return Path(str(value)).expanduser()


@dataclass
class _SidecarRun:
    run_id: str
    session_id: str
    task_id: str
    status: str
    thread: threading.Thread | None = None
    result: dict[str, Any] | None = None
    error: BaseException | None = None


@dataclass
class _SidecarTestHost:
    status: str
    project_root: Path

    def start_run(self, goal: str, *, config: ProductionConfig) -> Any:
        return self._run_result(goal, config=config, resumed_from=None)

    def resume_run(self, session_id: str, goal: str, *, config: ProductionConfig) -> Any:
        return self._run_result(goal, config=config, resumed_from=session_id)

    def _run_result(
        self,
        goal: str,
        *,
        config: ProductionConfig,
        resumed_from: str | None,
    ) -> _SidecarTestRunResult:
        model = str(getattr(config, "model", "") or "")
        suffix = f" model={_REDACTOR.redact_text(model)}" if model else ""
        resume_prefix = f"resume {resumed_from}: " if resumed_from else ""
        return _SidecarTestRunResult(
            run_id="run_sidecar_test",
            session_id=resumed_from or "session_sidecar_test",
            task_id="task_sidecar_test",
            status=self.status,
            final_answer=f"sidecar {self.status}: {resume_prefix}{_REDACTOR.redact_text(goal)}{suffix}",
            trace_run_dir=self.project_root / "work" / "traces" / "runs" / "run_sidecar_test",
        )

    def events(self, run_id: str) -> list[Any]:
        return [
            _SidecarTestEvent(
                event_id="event_sidecar_test",
                event_type="lifecycle.run.completed",
                run_id=run_id,
                component="kernel",
                severity="info",
                summary=f"sidecar {self.status}",
                sequence=0,
            )
        ]

    def snapshot(self, run_id: str) -> Any:
        return _SidecarTestSnapshot(
            run_id=run_id,
            session_id="session_sidecar_test",
            task_id="task_sidecar_test",
            status=self.status,
            trace_run_dir=self.project_root / "work" / "traces" / "runs" / run_id,
        )

    def cancel_run(self, run_id: str) -> Any:
        return _SidecarTestSnapshot(
            run_id=run_id,
            session_id="session_sidecar_test",
            task_id="task_sidecar_test",
            status="cancelled",
            trace_run_dir=self.project_root / "work" / "traces" / "runs" / run_id,
        )


@dataclass(frozen=True)
class _SidecarTestRunResult:
    run_id: str
    session_id: str
    task_id: str
    status: str
    final_answer: str
    trace_run_dir: Path

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "status": self.status,
            "final_answer": self.final_answer,
            "snapshot": self.snapshot().to_dict(),
        }

    def snapshot(self) -> _SidecarTestSnapshot:
        return _SidecarTestSnapshot(
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=self.task_id,
            status=self.status,
            trace_run_dir=self.trace_run_dir,
        )


@dataclass(frozen=True)
class _SidecarTestSnapshot:
    run_id: str
    session_id: str
    task_id: str
    status: str
    trace_run_dir: Path

    def to_dict(self) -> dict[str, Any]:
        return {
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "status": self.status,
            "trace_run_dir": str(self.trace_run_dir),
            "event_count": 1,
            "artifact_count": 0,
            "last_sequence": 0,
        }


@dataclass(frozen=True)
class _SidecarTestEvent:
    event_id: str
    event_type: str
    run_id: str
    component: str
    severity: str
    summary: str
    sequence: int

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_id": self.event_id,
            "event_type": self.event_type,
            "run_id": self.run_id,
            "component": self.component,
            "severity": self.severity,
            "summary": self.summary,
            "sequence": self.sequence,
        }


if __name__ == "__main__":
    main()
