#!/usr/bin/env bash
# checker.sh：invoice-bug 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"
python -m unittest discover -s tests -v

# CLI 冒烟：参数与输出格式保持（instruction 验收标准 3）。
output=$(python -m invoice.cli lamp 19.99 2 2>/dev/null) || {
    echo "REQUIRED: python -m invoice.cli must exit 0" >&2
    exit 1
}
echo "$output" | grep -q "subtotal:" || {
    echo "REQUIRED: invoice.cli output must contain subtotal line" >&2
    exit 1
}
