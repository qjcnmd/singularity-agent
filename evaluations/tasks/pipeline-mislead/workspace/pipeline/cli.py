"""报表渲染与命令行入口。"""

import argparse
import sys

from .aggregate import aggregate_by_date
from .loader import load_records


def render_report(totals: list[tuple[str, float]]) -> str:
    """渲染 `日期 总数` 行列表。"""
    return "\n".join(f"{date} {total:g}" for date, total in totals)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pipeline.cli")
    parser.add_argument("csv", help="path to the sales CSV file")
    args = parser.parse_args(argv)

    records = load_records(args.csv)
    totals = aggregate_by_date(records)
    print(render_report(totals))
    return 0


if __name__ == "__main__":
    sys.exit(main())
