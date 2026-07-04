from __future__ import annotations

import hashlib
import json
import time
from dataclasses import asdict, is_dataclass
from enum import Enum
from pathlib import Path
from typing import Any


def to_plain_data(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return value.as_posix()
    if is_dataclass(value) and not isinstance(value, type):
        return {key: to_plain_data(item) for key, item in asdict(value).items()}
    if isinstance(value, list):
        return [to_plain_data(item) for item in value]
    if isinstance(value, tuple):
        return [to_plain_data(item) for item in value]
    if isinstance(value, set):
        return sorted(to_plain_data(item) for item in value)
    if isinstance(value, dict):
        return {str(key): to_plain_data(item) for key, item in value.items()}
    return value


def utc_timestamp() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def stable_hash_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def stable_hash_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def stable_hash_payload(value: Any) -> str:
    text = json.dumps(to_plain_data(value), ensure_ascii=False, sort_keys=True, default=str)
    return stable_hash_text(text)
