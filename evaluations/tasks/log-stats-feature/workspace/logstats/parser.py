"""日志行解析：ISO 时间戳 + 级别 + 消息。"""

from dataclasses import dataclass
from datetime import datetime

LOG_LEVELS = ("INFO", "WARN", "ERROR")


@dataclass(frozen=True)
class LogEntry:
    """一条已解析的日志记录。"""

    timestamp: datetime
    level: str
    message: str


def parse_line(line: str) -> LogEntry | None:
    """解析单行日志；格式不符返回 None（调用方跳过，不崩溃）。

    日志行格式：`YYYY-MM-DD HH:MM:SS LEVEL message`（单空格分隔）。
    """
    line = line.rstrip("\n")
    # 时间戳 `YYYY-MM-DD HH:MM:SS` 含空格，整体占两个 token。
    parts = line.split(" ", 3)
    if len(parts) < 4:
        return None
    timestamp_text = f"{parts[0]} {parts[1]}"
    level = parts[2]
    message = parts[3]
    if level not in LOG_LEVELS:
        return None
    try:
        timestamp = datetime.strptime(timestamp_text, "%Y-%m-%d %H:%M:%S")
    except ValueError:
        return None
    return LogEntry(timestamp=timestamp, level=level, message=message)


def parse_lines(lines: list[str]) -> list[LogEntry]:
    """解析多行；跳过格式错误的行。"""
    return [entry for line in lines if (entry := parse_line(line)) is not None]
