"""cachestore 命令行入口：从 stdin 读取操作序列并逐行执行、输出结果。

每行一个操作，空格分隔：
- ``capacity <n>``              设置容量（读取阶段的配置行）
- ``default_ttl <secs>``        设置默认 TTL（读取阶段的配置行，可选）
- ``set <key> <value> [ttl]``
- ``get <key>``                 输出 ``get <key> = <value>`` 或 ``get <key> = None``
- ``delete <key>``
- ``len``                       输出 ``len = <n>``

前若干行为配置行，必须含 ``capacity <n>``，可含可选的 ``default_ttl <secs>``（顺序任意，
且必须出现在任何操作行之前）。配置行之后的非法行一律忽略；操作阶段遇到 ``ValueError``
输出 ``ERROR: <message>`` 并继续处理后续行。进程退出码恒为 0。
"""

from __future__ import annotations

import sys

from .cache import Cache


def parse_float(token: str) -> float:
    return float(token)


def main(argv: list[str] | None = None) -> int:
    del argv  # stdin 驱动，不使用 argv。
    lines = [ln.rstrip("\n") for ln in sys.stdin]

    capacity: int | None = None
    default_ttl: float | None = None
    idx = 0
    # 读取阶段：仅从文件开头的连续行中识别配置行（不跨过操作行）。
    while idx < len(lines):
        tok = lines[idx].strip().split()
        if not tok:
            idx += 1
            continue
        cmd = tok[0]
        if cmd == "capacity" and len(tok) >= 2:
            capacity = int(tok[1])
        elif cmd == "default_ttl" and len(tok) >= 2:
            default_ttl = parse_float(tok[1])
        else:
            break  # 遇到操作行，退出配置阶段。
        idx += 1

    if capacity is None:
        print("ERROR: missing capacity line")
        return 0

    cache = Cache(capacity=capacity, default_ttl_seconds=default_ttl)

    for raw in lines[idx:]:
        tok = raw.strip().split()
        if not tok:
            continue
        cmd = tok[0]
        try:
            if cmd == "set" and len(tok) >= 3:
                ttl = parse_float(tok[3]) if len(tok) >= 4 else None
                cache.set(tok[1], tok[2], ttl)
            elif cmd == "get" and len(tok) >= 2:
                value = cache.get(tok[1])
                print(f"get {tok[1]} = {value}")
            elif cmd == "delete" and len(tok) >= 2:
                cache.delete(tok[1])
            elif cmd == "len":
                print(f"len = {len(cache)}")
        except ValueError as exc:
            print(f"ERROR: {exc}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
