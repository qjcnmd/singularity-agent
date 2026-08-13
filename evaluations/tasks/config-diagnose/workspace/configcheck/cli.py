"""configcheck 命令行入口。"""

import argparse
import sys

from .loader import ConfigError
from .validator import run_validation


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="configcheck.cli")
    parser.add_argument("--verbose", action="store_true", help="print settings summary")
    args = parser.parse_args(argv)

    try:
        problems = run_validation()
    except ConfigError as error:
        print(f"ERROR: {error}")
        return 2

    if problems:
        for problem in problems:
            print(f"ERROR: {problem}")
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
