from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

from singularity.utils.serialization import (
    stable_hash_bytes,
    stable_hash_text,
    to_plain_data,
    utc_timestamp,
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
    assert utc_timestamp().endswith("Z")


def test_stable_hash_text_is_deterministic() -> None:
    assert stable_hash_text("same text") == stable_hash_text("same text")
    assert stable_hash_text("same text") != stable_hash_text("other text")
    assert stable_hash_bytes(b"same text") == stable_hash_text("same text")
