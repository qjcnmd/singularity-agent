from __future__ import annotations

import json
from pathlib import Path

from scripts.export_rust_parity_fixtures import build_fixtures


def test_rust_parity_fixture_shape_and_tool_payload_boundary() -> None:
    fixtures = build_fixtures()

    payload = fixtures["tool_observation_model_payload"]
    serialized = json.dumps(payload, sort_keys=True)

    assert payload["tool_call_id"] == "call_1"
    assert "policy_decision_id" not in payload
    assert "approval_grant_id" not in payload
    assert "metadata" not in payload
    assert "raw_arguments" not in serialized


def test_checked_in_rust_parity_fixture_is_current() -> None:
    path = Path("tests/fixtures/rust_parity/python_oracle.json")
    assert path.exists()
    current = json.loads(path.read_text(encoding="utf-8"))
    assert current == build_fixtures()
