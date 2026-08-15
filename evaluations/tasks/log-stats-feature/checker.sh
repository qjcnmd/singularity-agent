#!/usr/bin/env bash
# checker.sh：log-stats-feature 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"
python -m unittest discover -s tests -v

# CLI 冒烟：--hourly 对 sample.log 输出小时桶（instruction 验收标准 3）。
output=$(python -m logstats.cli --hourly sample.log 2>/dev/null) || {
    echo "REQUIRED: python -m logstats.cli --hourly sample.log must exit 0" >&2
    exit 1
}
echo "$output" | grep -Eq "[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:00" || {
    echo "REQUIRED: --hourly output must contain YYYY-MM-DD HH:00 bucket" >&2
    exit 1
}
