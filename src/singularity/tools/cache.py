from __future__ import annotations

import threading
import time
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from singularity.tools.models import ToolResult


@dataclass
class _CacheEntry:
    result: ToolResult
    created_at: float
    touched_paths: tuple[str, ...]


class ToolResultCache:
    def __init__(self) -> None:
        self._entries: OrderedDict[str, _CacheEntry] = OrderedDict()
        self._lock = threading.RLock()

    def get(self, key: str, *, ttl_seconds: float | None) -> ToolResult | None:
        with self._lock:
            entry = self._entries.get(key)
            if entry is None:
                return None
            if ttl_seconds is not None and time.time() - entry.created_at > ttl_seconds:
                self._entries.pop(key, None)
                return None
            self._entries.move_to_end(key)
            return entry.result.model_copy(deep=True)

    def set(
        self,
        key: str,
        result: ToolResult,
        *,
        max_entries: int,
        touched_paths: tuple[str, ...],
    ) -> None:
        with self._lock:
            self._entries[key] = _CacheEntry(
                result=result.model_copy(deep=True),
                created_at=time.time(),
                touched_paths=touched_paths,
            )
            self._entries.move_to_end(key)
            while len(self._entries) > max_entries:
                self._entries.popitem(last=False)

    def invalidate_paths(self, paths: list[str]) -> None:
        with self._lock:
            normalized = {Path(path).as_posix() for path in paths}
            for key, entry in list(self._entries.items()):
                if any(
                    _paths_overlap(changed, touched)
                    for changed in normalized
                    for touched in entry.touched_paths
                ):
                    self._entries.pop(key, None)

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()

    def __len__(self) -> int:
        with self._lock:
            return len(self._entries)


def _paths_overlap(left: str, right: str) -> bool:
    left_parts = PurePosixPath(Path(left).as_posix()).parts
    right_parts = PurePosixPath(Path(right).as_posix()).parts
    return (
        left_parts == right_parts
        or left_parts[: len(right_parts)] == right_parts
        or right_parts[: len(left_parts)] == left_parts
    )
