from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

from singularity.utils.serialization import (
    coerce_dict,
    coerce_evaluation_dict,
    coerce_float,
    coerce_int,
    enum_value,
    stable_hash_bytes,
    stable_hash_text,
    to_plain_data,
    utc_iso_timestamp,
    utc_z_timestamp,
)


class _Status(StrEnum):
    READY = "ready"


@dataclass(frozen=True)
class _Payload:
    path: Path
    status: _Status
    tags: set[str]


def test_to_plain_data_handles_nested_runtime_values() -> None:
    payload = _Payload(
        path=Path("src/app.py"),
        status=_Status.READY,
        tags={"b", "a"},
    )

    assert to_plain_data({"payload": payload}) == {
        "payload": {
            "path": "src/app.py",
            "status": "ready",
            "tags": ["a", "b"],
        }
    }


def test_utc_timestamp_uses_utc_z_suffix() -> None:
    assert utc_z_timestamp().endswith("Z")
    assert "+00:00" in utc_iso_timestamp()


def test_stable_hash_text_is_deterministic() -> None:
    assert stable_hash_text("same text") == stable_hash_text("same text")
    assert stable_hash_text("same text") != stable_hash_text("other text")
    assert stable_hash_bytes(b"same text") == stable_hash_text("same text")


def test_enum_value_and_coercion_helpers_preserve_existing_defaults() -> None:
    assert enum_value(_Status.READY) == "ready"
    assert enum_value("ready") == "ready"
    assert coerce_int("bad") == 0
    assert coerce_float("bad") == 0.0
    assert coerce_dict({"a": 1}, "payload") == {"a": 1}


def test_evaluation_dict_helper_preserves_error_message_and_mapping_copy() -> None:
    source = {"a": 1}

    assert coerce_evaluation_dict(source, "summary") == {"a": 1}
    assert coerce_evaluation_dict(None, "summary", allow_none=True) == {}

    try:
        coerce_evaluation_dict("bad", "summary")
    except ValueError as exc:
        assert str(exc) == "evaluation summary must be an object."
    else:
        raise AssertionError("Expected ValueError for non-dict evaluation payload.")
