"""通话记录数据模型。"""

from dataclasses import dataclass
from datetime import datetime


@dataclass(frozen=True)
class CallRecord:
    """一笔通话记录。

    计费规则以 start 为通话开始时刻，持续 minutes 分钟，
    category 决定该笔通话的每分钟基础费率。

    Attributes:
        start: 通话开始时刻（本地时间）。
        minutes: 通话持续分钟数（正整数）。
        category: 通话类别（``standard`` 或 ``premium``）。
    """

    start: datetime
    minutes: int
    category: str
