#!/usr/bin/env bash
# checker.sh：cart-refactor 判分脚本。
# exit 0 = 通过；1 = 失败；2 = 部分（行为全绿但重构要求未满足）。
set -e
cd "$(dirname "$0")/workspace"

# 1) 行为测试必须全绿。
python -m unittest discover -s tests -v

# 2) 重构结构要求。
money_file="shopping/money.py"
if [ ! -f "$money_file" ]; then
    echo "REQUIRED: shopping/money.py is missing" >&2
    exit 2
fi
if ! python - "$money_file" << 'PYEOF'
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("shopping.money", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert callable(getattr(module, "format_amount")), "money.format_amount missing"
assert isinstance(getattr(module, "DISCOUNT_THRESHOLDS"), tuple), "money.DISCOUNT_THRESHOLDS missing"
assert module.DISCOUNT_THRESHOLDS == ((500, 0.9), (100, 0.95)), "DISCOUNT_THRESHOLDS content mismatch"
PYEOF
then
    echo "REQUIRED: money.py contract not satisfied" >&2
    exit 2
fi

# 3) 三个模块不再各自定义金额格式化函数（复用 money.format_amount）。
for module in cart discounts receipt; do
    if grep -q "def format_cents\|def fmt_money\|def money_str" "shopping/${module}.py"; then
        echo "REQUIRED: shopping/${module}.py still defines its own money formatter" >&2
        exit 2
    fi
done

echo "REFACTOR OK"
exit 0
