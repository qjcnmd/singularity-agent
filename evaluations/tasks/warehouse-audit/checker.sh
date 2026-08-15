#!/usr/bin/env bash
# checker.sh：warehouse-audit 判分脚本。
# 退出码：0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"

# 1) 全部单元测试必须通过。
python -m unittest discover -s tests -v

# 2) 必须新增回归测试文件。
[ -f tests/test_regression.py ] || {
    echo "REQUIRED: tests/test_regression.py missing" >&2
    exit 1
}

# 3) 运行时构造 fixture，验证 CLI 真实行为（防止只修测试/特判）。
#    fixture 故意包含：乱序表头列、混合日期格式、至少一条 OUT 记录。
python - <<'PYEOF'
import subprocess
import sys
import tempfile
import os

os.chdir(os.getcwd())
sys.path.insert(0, ".")

# 用纯文本手工拼一份 CSV，避免引入 csv 模块写出的顺序依赖。
# 表头顺序故意与 cli 假定的 sku,qty,kind,date,... 不同，且混用日期格式。
header = "name,sku,date,kind,qty,unit_price"
rows = [
    "Widget,A,2026-08-01,IN,10,12.50",
    "Gadget,B,2026-8-2,IN,20,3.00",
    "Widget,A,2026-8-2,IN,5,12.50",
    "Widget,A,2026-08-02,OUT,3,12.50",
    "Gadget,B,2026-8-3,OUT,7,3.00",
    "Widget,A,2026-08-10,IN,4,12.50",
]
csv_text = "\n".join([header] + rows) + "\n"

with tempfile.NamedTemporaryFile(
    "w", suffix=".csv", encoding="utf-8", newline="", delete=False
) as f:
    f.write(csv_text)
    fixture = f.name

try:
    # 直接在当前工作目录运行模块，确保能 import 到 warehouse 包。
    proc = subprocess.run(
        [sys.executable, "-m", "warehouse.cli", fixture],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
finally:
    os.unlink(fixture)

if proc.returncode != 0:
    print("REQUIRED: warehouse.cli must exit 0")
    print(proc.stderr)
    sys.exit(1)
output = proc.stdout

# 解析 stock 段，校验含 OUT 的 SKU 结余正确（出库应减少库存）。
stock = {}
in_stock = False
for line in output.splitlines():
    if line.strip() == "stock:":
        in_stock = True
        continue
    if line.strip() == "history:":
        in_stock = False
        continue
    if in_stock:
        parts = line.split("\t")
        if len(parts) >= 4:
            stock[parts[0]] = int(parts[3])

# A：1000 +10 +5 -3 +4 = 1016；B：1000 +20 -7 = 1013。
if stock.get("A") != 1016:
    print("REQUIRED: stock of A should be 1016 (OUT must reduce stock), got", stock)
    sys.exit(1)
if stock.get("B") != 1013:
    print("REQUIRED: stock of B should be 1013, got", stock)
    sys.exit(1)

# 解析 history 段，校验日期按真实时间排序（2026-8-2 < 2026-08-03 < 2026-08-10）。
dates = []
in_hist = False
for line in output.splitlines():
    if line.strip() == "history:":
        in_hist = True
        continue
    if in_hist and line.strip():
        parts = line.split("\t")
        if parts:
            dates.append(parts[0])

expected = ["2026-08-01", "2026-08-02", "2026-08-02",
            "2026-08-02", "2026-08-03", "2026-08-10"]
if dates != expected:
    print("REQUIRED: history must be sorted by real date, normalized to YYYY-MM-DD")
    print("expected:", expected)
    print("got:     ", dates)
    sys.exit(1)

print("CLI behavior verification passed")
PYEOF

echo "ALL CHECKS PASSED"
