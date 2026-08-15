"""命令行入口：读取通话记录 CSV，输出账单。

CSV 每行格式：``start,minutes,category``
  - ``start``：通话开始时刻，``YYYY-MM-DD HH:MM``
  - ``minutes``：持续分钟数（正整数）
  - ``category``：``standard`` 或 ``premium``

输出每笔通话一行明细，最后一行以 ``total: <金额>`` 给出账单总额。
"""

import argparse
import csv
import sys
from datetime import datetime

from .calculator import call_cost, total_cost
from .models import CallRecord

_START_FORMAT = "%Y-%m-%d %H:%M"


def parse_calls(path: str) -> list[CallRecord]:
    """从 CSV 文件解析通话记录列表。"""
    calls = []
    with open(path, "r", newline="", encoding="utf-8") as handle:
        reader = csv.reader(handle)
        for lineno, row in enumerate(reader, start=1):
            if not row:
                continue
            if len(row) != 3:
                raise ValueError(
                    f"{path}:{lineno}: 每行应有 3 列（start,minutes,category），"
                    f"实际 {len(row)} 列"
                )
            start_str, minutes_str, category = (cell.strip() for cell in row)
            start = datetime.strptime(start_str, _START_FORMAT)
            minutes = int(minutes_str)
            if minutes <= 0:
                raise ValueError(f"{path}:{lineno}: minutes 必须为正整数")
            calls.append(CallRecord(start=start, minutes=minutes, category=category))
    return calls


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="billing.cli")
    parser.add_argument("csv", help="通话记录 CSV 文件路径")
    args = parser.parse_args(argv)

    try:
        calls = parse_calls(args.csv)
    except (OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if not calls:
        print("error: 没有可计费的通话记录", file=sys.stderr)
        return 2

    for call in calls:
        print(f"{call.start.strftime(_START_FORMAT)} {call.minutes:>4} {call.category:<8} {call_cost(call):.2f}")
    print(f"total: {total_cost(calls):.2f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
