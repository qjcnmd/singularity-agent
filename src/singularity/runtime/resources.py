from __future__ import annotations

from typing import Any


def close_runtime_resources(kernel: Any | None) -> bool:
    if kernel is None:
        return False
    close_resources = getattr(kernel, "close_resources", None)
    if not callable(close_resources):
        return False
    close_resources()
    return True
