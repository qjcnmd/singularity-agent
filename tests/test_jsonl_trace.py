from __future__ import annotations

import json

from singularity.jsonl_trace import JsonlTraceRecorder


def test_jsonl_trace_recorder_redacts_payload_and_writes_jsonl(tmp_path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)

    trace.record("secret.event", {"token": "secret-token", "safe": "value"})

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    assert events[0]["event"] == "secret.event"
    assert events[0]["data"]["token"] == "<redacted>"
    assert events[0]["data"]["safe"] == "value"
