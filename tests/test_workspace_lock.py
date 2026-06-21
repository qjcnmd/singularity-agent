from __future__ import annotations

import json
from multiprocessing import Queue, Process
import os
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from singularity.kernel.exceptions import WorkspaceLockError
from singularity.kernel.locks import WorkspaceLockManager


def test_workspace_lock_blocks_second_writer(tmp_path: Path) -> None:
    first = WorkspaceLockManager(tmp_path)
    second = WorkspaceLockManager(tmp_path)

    first.acquire_lock(run_id="run_1", read_only=False)

    with pytest.raises(WorkspaceLockError):
        second.acquire_lock(run_id="run_2", read_only=False)

    first.release_lock()
    assert not (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()


def test_workspace_lock_allows_read_only_shared_locks(tmp_path: Path) -> None:
    first = WorkspaceLockManager(tmp_path)
    second = WorkspaceLockManager(tmp_path)

    first.acquire_lock(run_id="run_1", read_only=True)
    second.acquire_lock(run_id="run_2", read_only=True)

    payload = json.loads((tmp_path / ".singularity" / "locks" / "workspace.lock").read_text())
    assert sorted(holder["run_id"] for holder in payload["holders"]) == ["run_1", "run_2"]

    second.release_lock()
    first.release_lock()


def test_workspace_lock_detects_and_replaces_stale_lock(tmp_path: Path) -> None:
    lock_path = tmp_path / ".singularity" / "locks" / "workspace.lock"
    lock_path.parent.mkdir(parents=True)
    stale_time = (datetime.now(UTC) - timedelta(hours=2)).isoformat()
    lock_path.write_text(
        json.dumps(
            {
                "version": 1,
                "mode": "write",
                "holders": [
                    {
                        "run_id": "old_run",
                        "pid": 99999999,
                        "hostname": "stale",
                        "acquired_at": stale_time,
                        "updated_at": stale_time,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    manager = WorkspaceLockManager(tmp_path, stale_after_seconds=1)

    assert manager.detect_stale_lock() is True
    manager.acquire_lock(run_id="new_run", read_only=False)

    payload = json.loads(lock_path.read_text(encoding="utf-8"))
    assert payload["holders"][0]["run_id"] == "new_run"
    assert payload["holders"][0]["pid"] == os.getpid()
    assert manager.last_stale_lock_detected is True


def test_workspace_lock_remembers_stale_lock_removed_during_acquire(tmp_path: Path) -> None:
    lock_path = tmp_path / ".singularity" / "locks" / "workspace.lock"
    lock_path.parent.mkdir(parents=True)
    stale_time = (datetime.now(UTC) - timedelta(hours=2)).isoformat()
    lock_path.write_text(
        json.dumps(
            {
                "version": 1,
                "mode": "write",
                "holders": [
                    {
                        "run_id": "old_run",
                        "pid": 99999999,
                        "hostname": "stale",
                        "acquired_at": stale_time,
                        "updated_at": stale_time,
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    manager = WorkspaceLockManager(tmp_path, stale_after_seconds=1)

    manager.acquire_lock(run_id="new_run", read_only=False)

    assert manager.detect_stale_lock() is False
    assert manager.last_stale_lock_detected is True


def test_workspace_lock_allows_only_one_writer_across_processes(tmp_path: Path) -> None:
    queue: Queue[str] = Queue()
    processes = [
        Process(target=_try_acquire_writer, args=(tmp_path, f"run_{index}", queue))
        for index in range(8)
    ]

    for process in processes:
        process.start()
    for process in processes:
        process.join(timeout=5)

    results = [queue.get(timeout=1) for _ in processes]
    assert results.count("acquired") == 1


def _try_acquire_writer(path: Path, run_id: str, queue: Queue[str]) -> None:
    try:
        WorkspaceLockManager(path).acquire_lock(run_id=run_id, read_only=False)
    except WorkspaceLockError:
        queue.put("locked")
    except Exception as exc:
        queue.put(type(exc).__name__)
    else:
        queue.put("acquired")
