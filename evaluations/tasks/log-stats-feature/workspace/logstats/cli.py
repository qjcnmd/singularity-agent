"""日志统计工具命令行入口。"""

import argparse
import sys

from .aggregate import count_by_level
from .parser import parse_lines


def read_lines(path: str) -> list[str]:
    """读取日志文件全部行。"""
    with open(path, encoding="utf-8") as handle:
        return handle.readlines()


def render_by_level(counts: dict[str, int]) -> str:
    """按级别统计的文本输出。"""
    return "\n".join(f"{level}: {count}" for level, count in counts.items())


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="logstats.cli")
    parser.add_argument("--hourly", action="store_true", help="aggregate by hour bucket")
    parser.add_argument("logfile", help="path to the log file")
    args = parser.parse_args(argv)

    lines = read_lines(args.logfile)
    entries = parse_lines(lines)
    if args.hourly:
        print("--hourly is not implemented yet")
        return 1
    print(render_by_level(count_by_level(entries)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
