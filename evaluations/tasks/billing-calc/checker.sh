#!/usr/bin/env bash
# checker.sh：billing-calc 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"

# ---------------------------------------------------------------------------
# 1) 单元测试必须全部通过。
# ---------------------------------------------------------------------------
python -m unittest discover -s tests -v

# ---------------------------------------------------------------------------
# 2) 随机化行为验证（防特判）。
#    这里在 caller 的进程里内嵌一份**独立**的参考实现（不 import workspace
#    的任何代码），按规格逐分钟计费，再与 workspace 的实际输出比对。
#    覆盖：20 组随机通话 + 一组确定性边界用例（夜间整点 / 跨天 / 舍入）。
# ---------------------------------------------------------------------------
python - <<'PY'
import csv, os, random, subprocess, sys, tempfile
from datetime import datetime, timedelta

sys.path.insert(0, os.getcwd())

# ---- 独立参考实现（按规格：逐分钟费率 + 单笔一次舍入，总计不二次舍入）----
def per_min(dt, category):
    rate = 0.5 if category == "standard" else 0.8
    # 夜间 22:00(含)-07:00(不含)
    if dt.hour >= 22 or dt.hour < 7:
        rate *= 0.5
    return rate

def ref_call_cost(start, minutes, category):
    raw = 0.0
    for i in range(minutes):
        raw += per_min(start + timedelta(minutes=i), category)
    factor = 0.90 if raw >= 200 else (0.95 if raw >= 100 else 1.0)
    return round(raw * factor, 2)

def ref_total(calls):
    return sum(ref_call_cost(*c) for c in calls)

FAILURES = []

def close(a, b):
    return abs(a - b) < 0.004   # 1 分容差：per-minute 浮点差异不应触发误报

def write_csv(calls):
    fd, path = tempfile.mkstemp(suffix=".csv", text=True)
    with os.fdopen(fd, "w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f)
        for start, minutes, category in calls:
            writer.writerow([start.strftime("%Y-%m-%d %H:%M"), str(minutes), category])
    return path

# ---- 通过公开接口（billing.calculator）比对 ----
from billing.calculator import call_cost, total_cost

def check_calls(calls, label):
    # 1) 公开接口逐笔比对
    for start, minutes, category in calls:
        expected = ref_call_cost(start, minutes, category)
        actual = call_cost(CallRecord(start, minutes, category))
        if not close(expected, actual):
            FAILURES.append(f"[{label}] call_cost mismatch: {start} {minutes}m {category}"
                            f" expected={expected} actual={actual}")
    # 2) 公开接口总额比对
    recs = [CallRecord(s, m, c) for s, m, c in calls]
    if not close(ref_total(calls), total_cost(recs)):
        FAILURES.append(f"[{label}] total_cost mismatch: "
                        f"expected={ref_total(calls)} actual={total_cost(recs)}")
    # 3) CLI 输出比对（逐笔明细 + total 行）
    csv_path = write_csv(calls)
    proc = subprocess.run(
        [sys.executable, "-m", "billing.cli", csv_path],
        cwd=os.getcwd(),
        # stderr 编码随平台而变，判分只依赖 stdout；失败时由本脚本自行报告。
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, encoding="utf-8",
    )
    os.unlink(csv_path)
    lines = proc.stdout.strip().splitlines()
    if not lines or not lines[-1].startswith("total:"):
        FAILURES.append(f"[{label}] CLI output missing total line")
        return
    cli_total = float(lines[-1].split(":", 1)[1].strip())
    if not close(ref_total(calls), cli_total):
        FAILURES.append(f"[{label}] CLI total mismatch: "
                        f"expected={ref_total(calls)} actual={cli_total}")

# 任意记录结构（与 billing.models 解耦，避免拷贝）。
class CallRecord:
    def __init__(self, start, minutes, category):
        self.start = start
        self.minutes = minutes
        self.category = category

# ---- 2a) 20 组随机序列 ----
random.seed(20260815)
_CATS = ["standard", "premium"]
_EDGES = [(21,59),(22,0),(6,59),(7,0),(23,45),(0,0),(10,0),(15,30),(8,20), (3,10)]

for gi in range(20):
    group = []
    for _ in range(random.randint(3, 8)):
        day = random.randint(1, 28)
        h, m = random.choice(_EDGES)
        start = datetime(2026, 1, day, h, m)
        minutes = random.randint(1, 400)
        cat = random.choice(_CATS)
        group.append((start, minutes, cat))
    check_calls(group, f"random-{gi}")

# ---- 2b) 确定性边界 / 舍入用例（保证两种缺陷必须被检出）----
boundary = [
    (datetime(2026,1,10,10,0), 220, "standard"),   # 舍入：期望 104.50
    (datetime(2026,1,10,7,0),  1,   "standard"),   # 恰好 07:00：期望 0.50
    (datetime(2026,1,10,6,59), 1,   "standard"),   # 夜间：期望 0.25
    (datetime(2026,1,10,22,0), 1,   "premium"),    # 恰好 22:00：期望 0.40
    (datetime(2026,1,10,23,50),30,  "standard"),   # 跨午夜：期望 7.50
    (datetime(2026,1,10,21,55),10,  "standard"),   # 22:00 边界：期望 3.75
]
check_calls(boundary, "boundary")

if FAILURES:
    print("VALIDATION FAILED", file=sys.stderr)
    for line in FAILURES:
        print("  " + line, file=sys.stderr)
    sys.exit(1)
print("randomized behavioral validation: OK (20 groups + boundary suite)")
PY

# ---------------------------------------------------------------------------
# 3) CLI 冒烟：对样例 CSV 输出须含 total 行。
# ---------------------------------------------------------------------------
python - <<'PY'
import csv, os, subprocess, sys, tempfile
fd, path = tempfile.mkstemp(suffix=".csv", text=True)
with os.fdopen(fd, "w", newline="", encoding="utf-8") as f:
    f.write("2026-01-10 10:00,10,standard\n")
    f.write("2026-01-10 23:00,2,premium\n")
proc = subprocess.run([sys.executable, "-m", "billing.cli", path],
                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                      text=True, encoding="utf-8",
                      cwd=os.getcwd())
os.unlink(path)
if proc.returncode != 0:
    print("CLI smoke: exit != 0", file=sys.stderr)
    sys.exit(1)
if "total:" not in proc.stdout:
    print("CLI smoke: output missing 'total:' line", file=sys.stderr)
    sys.exit(1)
print("CLI smoke: OK")
PY

echo "checker: PASS"
