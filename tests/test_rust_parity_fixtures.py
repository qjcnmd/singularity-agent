from __future__ import annotations

import json
from pathlib import Path

from scripts.export_rust_parity_fixtures import build_fixtures


def test_rust_parity_fixture_shape_and_tool_payload_boundary() -> None:
    fixtures = build_fixtures()

    payload = fixtures["tool_result_payload"]
    output = fixtures["tool_output"]
    reference_output = fixtures["reference_tool_output"]
    serialized = json.dumps(payload, sort_keys=True)

    assert payload["tool_call_id"] == "call_1"
    assert set(output) == {"ok", "content", "error_code", "truncated", "metadata"}
    assert set(output["content"]) == {
        "preview",
        "artifact_ref",
        "artifact_refs",
        "result_id",
        "digest",
    }
    assert output["ok"] is True
    assert reference_output["content"]["preview"] is None
    assert reference_output["content"]["artifact_ref"] == "artifact_1"
    assert reference_output["truncated"] is True
    assert "policy_decision_id" not in payload
    assert "approval_grant_id" not in payload
    assert "metadata" not in payload
    assert "tool_call_id" not in output
    assert "tool_name" not in output
    assert "status" not in output
    assert "raw_arguments" not in serialized
    assert "ToolObservation" not in serialized
    assert "content_preview" not in serialized
    assert "content_digest" not in serialized
    assert "raw_result_ref" not in serialized
    assert "raw_arguments" not in json.dumps(output, sort_keys=True)


def test_checked_in_rust_parity_fixture_is_current() -> None:
    path = Path("tests/fixtures/rust_parity/python_oracle.json")
    assert path.exists()
    current = json.loads(path.read_text(encoding="utf-8"))
    assert current == build_fixtures()
