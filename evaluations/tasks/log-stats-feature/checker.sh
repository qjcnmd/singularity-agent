#!/usr/bin/env bash
# checker.sh：log-stats-feature 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"
python -m unittest discover -s tests -v
