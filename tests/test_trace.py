import json
from pathlib import Path

from singularity.trace import TraceWriter


def test_trace_writer_creates_jsonl_trace(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)

    trace.record("user_goal", {"goal": "learn the loop"})
    trace.record("final_answer", {"content": "done"})

    assert trace.path.parent == tmp_path / ".singularity" / "runs"
    assert trace.path.exists()

    lines = trace.path.read_text(encoding="utf-8").splitlines()
    events = [json.loads(line) for line in lines]
    assert [event["event"] for event in events] == ["user_goal", "final_answer"]
    assert {event["run_id"] for event in events} == {trace.run_id}
    assert events[0]["data"] == {"goal": "learn the loop"}
