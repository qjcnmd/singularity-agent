from __future__ import annotations

import hashlib
import json
import time
from dataclasses import asdict, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from pathlib import Path
from typing import Any, TypeVar

EnumT = TypeVar("EnumT", bound=Enum)


def to_plain_data(value: Any, *, path_style: str = "posix") -> Any:
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return str(value) if path_style == "str" else value.as_posix()
    if is_dataclass(value) and not isinstance(value, type):
        return {
            key: to_plain_data(item, path_style=path_style)
            for key, item in asdict(value).items()
        }
    if isinstance(value, list):
        return [to_plain_data(item, path_style=path_style) for item in value]
    if isinstance(value, tuple):
        return [to_plain_data(item, path_style=path_style) for item in value]
    if isinstance(value, set):
        return sorted(to_plain_data(item, path_style=path_style) for item in value)
    if isinstance(value, dict):
        return {
            str(key): to_plain_data(item, path_style=path_style)
            for key, item in value.items()
        }
    return value


def utc_timestamp() -> str:
    return utc_z_timestamp()


def utc_z_timestamp() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def utc_iso_timestamp() -> str:
    return datetime.now(UTC).isoformat()


def stable_hash_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def stable_short_hash_text(value: str, *, length: int = 16) -> str:
    return stable_hash_text(value)[:length]


def stable_hash_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def stable_hash_payload(value: Any) -> str:
    text = json.dumps(to_plain_data(value), ensure_ascii=False, sort_keys=True, default=str)
    return stable_hash_text(text)


def enum_value(value: Any) -> Any:
    return value.value if isinstance(value, Enum) else value


def enum_value_str(value: Any) -> str:
    return str(enum_value(value))


def coerce_enum(enum_type: type[EnumT], value: EnumT | str | Any, *, allow_name: bool = False) -> EnumT:
    if isinstance(value, enum_type):
        return value
    text = str(value)
    if allow_name and text in enum_type.__members__:
        return enum_type[text]
    return enum_type(text)


def coerce_enum_name(
    enum_type: type[EnumT],
    value: EnumT | str | Any,
    *,
    name_normalizer: Any = None,
) -> EnumT:
    if isinstance(value, enum_type):
        return value
    text = str(value)
    candidate = name_normalizer(text) if name_normalizer is not None else text
    try:
        return enum_type[candidate]
    except KeyError:
        return enum_type(text)


def coerce_optional_enum(enum_type: type[EnumT], value: Any, *, allow_name: bool = False) -> EnumT | None:
    if value is None:
        return None
    return coerce_enum(enum_type, value, allow_name=allow_name)


def coerce_int(value: Any, default: int = 0, *, bool_default: int | None = None) -> int:
    if isinstance(value, bool) and bool_default is not None:
        return bool_default
    try:
        return int(value or default)
    except (TypeError, ValueError):
        try:
            return int(float(str(value).strip()))
        except (TypeError, ValueError):
            return default


def coerce_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value or default)
    except (TypeError, ValueError):
        return default


def coerce_dict(
    value: Any,
    field_name: str = "value",
    *,
    allow_none: bool = False,
    error_message: str | None = None,
) -> dict[str, Any]:
    if value is None and allow_none:
        return {}
    if not isinstance(value, dict):
        raise ValueError(error_message or f"{field_name} must be an object.")
    return dict(value)
