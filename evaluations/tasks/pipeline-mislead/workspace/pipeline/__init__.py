"""数据管道：加载 → 聚合 → 报表。"""

from .aggregate import aggregate_by_date
from .loader import load_records

__all__ = [
    "aggregate_by_date",
    "load_records",
]
