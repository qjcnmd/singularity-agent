#!/usr/bin/env bash
# checker.sh：config-diagnose 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"
python -m unittest discover -s tests -v

# CLI 冒烟：不再报 configuration file not found，输出 OK（instruction 验收标准 2）。
output=$(python -m configcheck.cli 2>&1) || {
    echo "REQUIRED: python -m configcheck.cli must exit 0" >&2
    exit 1
}
echo "$output" | grep -q "OK" || {
    echo "REQUIRED: configcheck.cli output must contain OK" >&2
    exit 1
}
