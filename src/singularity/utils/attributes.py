from __future__ import annotations

from typing import Any


def nested_getattr(value: Any, path: str, *, default: Any = None) -> Any:
    current = value
    for name in path.split("."):
        if current is None:
            return default
        current = getattr(current, name, default)
        if current is default:
            return default
    return current
