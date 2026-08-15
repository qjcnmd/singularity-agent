"""带 TTL 的 LRU 缓存实现。

缓存使用 ``collections.OrderedDict`` 维护键的插入顺序（即最近使用顺序），
并同时记录每个键的过期时间。
"""

from __future__ import annotations

import time
from collections import OrderedDict


class Cache:
    """带 TTL 的 LRU 缓存。"""

    def __init__(
        self,
        capacity: int,
        default_ttl_seconds: float | None = None,
        clock=time.monotonic,
    ) -> None:
        if capacity < 1:
            raise ValueError("capacity must be >= 1")
        if default_ttl_seconds is not None and default_ttl_seconds <= 0:
            raise ValueError("default_ttl_seconds must be > 0 or None")
        self._capacity = capacity
        self._default_ttl = default_ttl_seconds
        self._now = clock
        self._data: "OrderedDict[str, tuple[str, float | None]]" = OrderedDict()

    def _expire_at(self, ttl: float | None) -> float | None:
        """把 TTL 期限换算为绝对过期时刻。"""
        if ttl is None:
            ttl = self._default_ttl
            if ttl is None:
                return None
        elif ttl <= 0:
            raise ValueError("ttl_seconds must be > 0 or None")
        return self._now() + ttl

    def _is_expired(self, key: str) -> bool:
        """判断某个键是否已过期（仅在键存在时调用）。"""
        _, expire_at = self._data[key]
        return expire_at is not None and self._now() >= expire_at

    def get(self, key: str) -> str | None:
        """命中返回 value，未命中返回 None。"""
        data = self._data
        if key not in data:
            return None
        data.move_to_end(key)
        value, _ = data[key]
        return value

    def set(
        self,
        key: str,
        value: str,
        ttl_seconds: float | None = None,
    ) -> None:
        """写入或更新一个条目。"""
        expire_at = self._expire_at(ttl_seconds)
        data = self._data
        if len(data) >= self._capacity:
            data.popitem(last=True)
        data[key] = (value, expire_at)
        data.move_to_end(key)

    def delete(self, key: str) -> bool:
        """删除一个条目，存在返回 True，否则返回 False。"""
        if key in self._data:
            del self._data[key]
            return True
        return False

    def __len__(self) -> int:
        """当前缓存中的条目数。"""
        return len(self._data)
