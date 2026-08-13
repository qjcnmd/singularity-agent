"""CSV 销售数据加载。"""

import csv
from pathlib import Path


def load_records(path: str | Path) -> list[dict[str, str]]:
    """读取 CSV 并返回记录列表（表头为键）。

    数据文件列名约定为 `date,product,count`。
    """
    with open(path, encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        return [dict(row) for row in reader]
