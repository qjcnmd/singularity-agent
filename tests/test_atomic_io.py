from __future__ import annotations

import json
import threading
from pathlib import Path

from singularity.atomic_io import atomic_write_text, file_lock


def test_atomic_write_text_replaces_file_without_reading_stale_temp(tmp_path: Path) -> None:
    target = tmp_path / "state.json"
    atomic_write_text(target, json.dumps({"version": 1}) + "\n")
    target.with_name(f".{target.name}.stale.tmp").write_text("PARTIAL_CORRUPT", encoding="utf-8")

    atomic_write_text(target, json.dumps({"version": 2}) + "\n")

    assert json.loads(target.read_text(encoding="utf-8")) == {"version": 2}
    assert "PARTIAL_CORRUPT" not in target.read_text(encoding="utf-8")


def test_file_lock_serializes_jsonl_appends(tmp_path: Path) -> None:
    path = tmp_path / "events.jsonl"
    errors: list[BaseException] = []

    def append(thread_index: int) -> None:
        try:
            for index in range(25):
                with file_lock(path) as handle:
                    handle.seek(0, 2)
                    line = json.dumps({"thread": thread_index, "index": index}) + "\n"
                    handle.write(line.encode("utf-8"))
                    handle.flush()
        except BaseException as exc:
            errors.append(exc)

    threads = [threading.Thread(target=append, args=(index,)) for index in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    lines = path.read_text(encoding="utf-8").splitlines()
    assert len(lines) == 100
    assert all(isinstance(json.loads(line), dict) for line in lines)
