from __future__ import annotations

from pathlib import Path

from singularity.kernel.recovery import CrashRecoveryManager


class TraceStore:
    def __init__(self) -> None:
        self.recovered = False

    def recover_incomplete_spans(self) -> list[str]:
        self.recovered = True
        return ["span_1"]


class Trace:
    def __init__(self) -> None:
        self.store = TraceStore()
        self.events: list[tuple[str, dict]] = []

    def record(self, event: str, data: dict) -> None:
        self.events.append((event, data))


class Lock:
    def detect_stale_lock(self) -> bool:
        return True


def test_crash_recovery_marks_stale_lock_and_incomplete_trace() -> None:
    trace = Trace()
    manager = CrashRecoveryManager(trace=trace, workspace_lock=Lock())

    report = manager.recover()

    assert report.recovered is True
    assert report.stale_lock_detected is True
    assert report.incomplete_trace_spans == ["span_1"]
    assert trace.events[0][0] == "recovery.detected"
    assert trace.events[-1][0] == "recovery.completed"


class WorkspaceState:
    def __init__(self, root: Path) -> None:
        self.workspace_root = root

    def recover_session(self):
        return {"status": "recoverable", "incomplete_transactions": ["tx_workspace"]}


class Sandbox:
    def __init__(self, root: Path) -> None:
        self.workspace_root = root


class Command:
    def __init__(self) -> None:
        self.stopped: list[str] = []

    def list_processes(self):
        return [type("Process", (), {"process_id": "proc_1", "status": "running"})()]

    def stop_process(self, process_id: str):
        self.stopped.append(process_id)


def test_crash_recovery_collects_and_cleans_mutation_sandbox_and_process_records(tmp_path: Path) -> None:
    journal = tmp_path / ".singularity" / "journals" / "tx_journal" / "journal.jsonl"
    journal.parent.mkdir(parents=True)
    journal.write_text('{"transaction_id":"tx_journal"}\n', encoding="utf-8")
    sandbox_root = tmp_path / "work" / "sandboxes" / "sandbox_leftover"
    sandbox_root.mkdir(parents=True)
    trace = Trace()
    command = Command()

    report = CrashRecoveryManager(
        trace=trace,
        workspace_state=WorkspaceState(tmp_path),
        sandbox=Sandbox(tmp_path),
        command=command,
    ).recover()

    assert report.recovered is True
    assert sorted(report.unfinished_mutations) == ["tx_journal", "tx_workspace"]
    assert report.leftover_sandboxes == [str(sandbox_root)]
    assert report.process_records == ["proc_1"]
    assert command.stopped == ["proc_1"]
    assert not sandbox_root.exists()
    assert (journal.parent / "recovered.json").exists()
