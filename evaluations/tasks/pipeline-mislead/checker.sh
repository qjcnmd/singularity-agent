#!/usr/bin/env bash
# checker.sh：pipeline-mislead 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"
python -m unittest discover -s tests -v

# CLI 冒烟：data/sales.csv 输出 `YYYY-MM-DD <count>`（instruction 验收标准 2）。
output=$(python -m pipeline.cli data/sales.csv 2>&1) || {
    echo "REQUIRED: python -m pipeline.cli data/sales.csv must exit 0" >&2
    exit 1
}
echo "$output" | grep -Eq "^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]+(\.[0-9]+)?$" || {
    echo "REQUIRED: pipeline.cli output must be lines of 'YYYY-MM-DD <count>'" >&2
    exit 1
}

# loader 独立健壮性断言：带空白表头的输入仍产出干净键。
# 防止只改 data/sales.csv 去掉表头空格绕过（instruction 验收标准 3：修复真实根因）。
python - <<'PYEOF'
import os
import sys
import tempfile

sys.path.insert(0, ".")
from pipeline.loader import load_records

handle = tempfile.NamedTemporaryFile("w", suffix=".csv", delete=False, newline="")
try:
    handle.write(" date,product, count\n2026-08-14,book,3\n")
    handle.close()
    records = load_records(handle.name)
    assert records, "loader returned no records"
    assert "date" in records[0] and "count" in records[0], (
        "loader must strip header whitespace (root-cause fix in parser, not data file)"
    )
    assert " date" not in records[0] and " count" not in records[0], (
        "loader must strip header whitespace"
    )
finally:
    os.unlink(handle.name)
PYEOF
