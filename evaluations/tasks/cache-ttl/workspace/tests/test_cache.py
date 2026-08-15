"""Cache 类单元测试（按 cachestore.cache 规格）。

本测试围绕 TTL 过期、LRU 淘汰与容量边界展开，使用可注入的假时钟保证确定性。
"""

from __future__ import annotations

import unittest

from cachestore.cache import Cache


class FakeClock:
    """可手动推进的假时钟（秒），用于确定性地模拟时间流逝。"""

    def __init__(self, start: float = 0.0) -> None:
        self.t = start

    def __call__(self) -> float:
        return self.t

    def advance(self, dt: float) -> None:
        self.t += dt


def make_cache(capacity, ttl=None):
    clock = FakeClock()
    return Cache(capacity, default_ttl_seconds=ttl, clock=clock), clock


class ConstructorTest(unittest.TestCase):
    def test_capacity_must_be_positive(self):
        for bad in (0, -1, -100):
            with self.assertRaises(ValueError):
                Cache(bad)

    def test_default_ttl_must_be_positive_or_none(self):
        Cache(2, default_ttl_seconds=None)
        Cache(2, default_ttl_seconds=10.0)
        for bad in (0, -1):
            with self.assertRaises(ValueError):
                Cache(2, default_ttl_seconds=bad)

class CacheBasicsTest(unittest.TestCase):
    def test_set_get_roundtrip(self):
        cache, _ = make_cache(5)
        cache.set("a", "1")
        cache.set("b", "2")
        self.assertEqual(cache.get("a"), "1")
        self.assertEqual(cache.get("b"), "2")

    def test_get_missing_returns_none(self):
        cache, _ = make_cache(5)
        self.assertIsNone(cache.get("nope"))

    def test_set_ttl_zero_raises(self):
        cache, _ = make_cache(5)
        with self.assertRaises(ValueError):
            cache.set("a", "1", ttl_seconds=0)

    def test_delete(self):
        cache, _ = make_cache(5)
        cache.set("a", "1")
        self.assertTrue(cache.delete("a"))
        self.assertFalse(cache.delete("a"))
        self.assertIsNone(cache.get("a"))

    def test_len_counts_live_entries(self):
        cache, _ = make_cache(5)
        self.assertEqual(len(cache), 0)
        cache.set("a", "1")
        cache.set("b", "2")
        self.assertEqual(len(cache), 2)
        cache.delete("a")
        self.assertEqual(len(cache), 1)


class TtlTest(unittest.TestCase):
    def test_get_returns_none_after_expiry(self):
        cache, clock = make_cache(5, ttl=10)
        cache.set("a", "1")
        self.assertEqual(cache.get("a"), "1")
        clock.advance(11)
        self.assertIsNone(cache.get("a"))

    def test_expired_entry_removed_from_len(self):
        cache, clock = make_cache(5, ttl=10)
        cache.set("a", "1")
        cache.set("b", "2")
        clock.advance(11)
        with_number = len(cache)
        self.assertEqual(with_number, 0)

    def test_explicit_ttl_overrides_default(self):
        cache, clock = make_cache(5, ttl=100)
        cache.set("a", "1", ttl_seconds=2)
        clock.advance(3)
        self.assertIsNone(cache.get("a"))
        cache.set("b", "2")  # 用默认 TTL
        clock.advance(10)
        self.assertEqual(cache.get("b"), "2")

    def test_update_refreshes_expiry(self):
        cache, clock = make_cache(5, ttl=10)
        cache.set("a", "1")
        clock.advance(8)
        cache.set("a", "new", ttl_seconds=5)  # 刷新过期时间
        clock.advance(4)
        self.assertEqual(cache.get("a"), "new")
        clock.advance(2)
        self.assertIsNone(cache.get("a"))

    def test_no_ttl_means_never_expire(self):
        cache, clock = make_cache(5, ttl=None)
        cache.set("a", "1")
        clock.advance(10_000_000)
        self.assertEqual(cache.get("a"), "1")


class LruEvictionTest(unittest.TestCase):
    def test_evicts_least_recently_used_when_full(self):
        cache, _ = make_cache(2)
        cache.set("a", "1")
        cache.set("b", "2")
        cache.set("c", "3")  # 淘汰最久未使用的 a
        self.assertIsNone(cache.get("a"))
        self.assertEqual(cache.get("b"), "2")
        self.assertEqual(cache.get("c"), "3")

    def test_get_refreshes_recency(self):
        cache, _ = make_cache(2)
        cache.set("a", "1")
        cache.set("b", "2")
        cache.get("a")  # a 变为最近使用
        cache.set("c", "3")  # 应淘汰 b
        self.assertEqual(cache.get("a"), "1")
        self.assertIsNone(cache.get("b"))
        self.assertEqual(cache.get("c"), "3")

    def test_update_moves_to_recent_position(self):
        cache, _ = make_cache(2)
        cache.set("a", "1")
        cache.set("b", "2")
        cache.set("a", "1b")  # 更新 a，a 变为最近使用
        cache.set("c", "3")  # 应淘汰 b
        self.assertEqual(cache.get("a"), "1b")
        self.assertIsNone(cache.get("b"))

    def test_update_does_not_grow_len(self):
        cache, _ = make_cache(2)
        cache.set("a", "1")
        cache.set("a", "2")
        self.assertEqual(len(cache), 1)


class ExpiredInLruTest(unittest.TestCase):
    def test_expired_entries_are_evicted_before_lru(self):
        cache, clock = make_cache(2, ttl=100)
        cache.set("a", "1")
        cache.set("b", "2")
        clock.advance(101)  # a、b 都过期
        cache.set("c", "3")
        self.assertEqual(len(cache), 1)  # 过期项不占容量

    def test_expired_entry_is_removed_before_lru_live_entry(self):
        # 无默认 TTL 构造，可得到“永不过期”的条目。
        cache, clock = make_cache(2, ttl=None)
        cache.set("a", "1")  # 永不过期
        cache.set("b", "2", ttl_seconds=100)  # 显式 TTL，100 秒后过期
        clock.advance(101)
        # 插入新键：应优先淘汰已过期的 b，而不是淘汰存活的 a。
        cache.set("c", "3")
        self.assertEqual(cache.get("a"), "1")
        self.assertIsNone(cache.get("b"))
        self.assertEqual(cache.get("c"), "3")


if __name__ == "__main__":
    unittest.main()
