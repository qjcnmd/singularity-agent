"""日志统计：按级别聚合。"""

from collections import Counter

from .parser import LOG_LEVELS, LogEntry


def count_by_level(entries: list[LogEntry]) -> dict[str, int]:
    """按日志级别统计条数（级别齐全，缺失为 0）。"""
    counts = Counter(entry.level for entry in entries)
    return {level: counts.get(level, 0) for level in LOG_LEVELS}
