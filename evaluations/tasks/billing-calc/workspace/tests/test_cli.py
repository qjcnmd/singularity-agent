"""CLI 测试：解析 CSV、逐笔明细行与账单总额输出。"""

import os
import subprocess
import sys
import tempfile
import unittest

# workspace 根目录（billing 包与 tests 所在目录）。
WORKSPACE_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run_cli(csv_text: str):
    """写入临时 CSV 并运行 billing.cli，返回 (退出码, 标准输出)。"""
    with tempfile.NamedTemporaryFile(
        "w", suffix=".csv", delete=False, newline="", encoding="utf-8"
    ) as handle:
        handle.write(csv_text)
        csv_path = handle.name
    try:
        result = subprocess.run(
            [sys.executable, "-m", "billing.cli", csv_path],
            cwd=WORKSPACE_ROOT,
            # stderr 编码随平台而变，测试只关心退出码与 stdout，故丢弃 stderr。
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
        )
        return result.returncode, result.stdout
    finally:
        os.unlink(csv_path)


class CliTotalTest(unittest.TestCase):
    def test_output_contains_total(self):
        code, out = run_cli("2026-01-10 10:00,10,standard\n")
        self.assertEqual(code, 0)
        self.assertIn("total: 5.00", out)

    def test_output_lists_each_call(self):
        code, out = run_cli(
            "2026-01-10 10:00,10,standard\n"
            "2026-01-10 23:00,2,premium\n"
        )
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(len(lines), 3)          # 2 笔明细 + total
        self.assertTrue(lines[-1].startswith("total:"))

    def test_total_matches_printout_of_rounding(self):
        # 与 test_calculator.RoundingTest 同一场景：220 分钟 standard 白天。
        code, out = run_cli("2026-01-10 10:00,220,standard\n")
        self.assertEqual(code, 0)
        self.assertIn("total: 104.50", out)


class CliErrorTest(unittest.TestCase):
    def test_bad_columns_exits_2(self):
        code, _ = run_cli("2026-01-10 10:00,10\n")  # 只有 2 列
        self.assertEqual(code, 2)

    def test_nonpositive_minutes_exits_2(self):
        code, _ = run_cli("2026-01-10 10:00,0,standard\n")
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
