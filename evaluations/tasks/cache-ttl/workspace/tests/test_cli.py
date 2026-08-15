"""cachestore CLI 测试：从 stdin 读取操作序列并断言输出。"""

from __future__ import annotations

import os
import subprocess
import sys
import unittest

# workspace 根目录（含 cachestore 包的目录）。
_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run_cli(ops: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "cachestore.cli"],
        input=ops,
        capture_output=True,
        text=True,
        cwd=_ROOT,
    )


class CliTest(unittest.TestCase):
    def test_basic_get(self):
        proc = run_cli("capacity 5\nset a 1\nget a\nlen\n")
        self.assertEqual(proc.returncode, 0)
        lines = proc.stdout.splitlines()
        self.assertEqual(lines, ["get a = 1", "len = 1"])

    def test_get_missing_emits_none(self):
        proc = run_cli("capacity 3\nget zzz\n")
        self.assertEqual(proc.stdout.strip(), "get zzz = None")

    def test_default_ttl_directive(self):
        # 首行 capacity，次行 default_ttl（顺序任意）。
        proc = run_cli("capacity 5\ndefault_ttl 2\nset a 1\nget a\nget b\n")
        lines = proc.stdout.splitlines()
        # CLI 用真实时钟，无法断言过期行为，只断言格式与可运行。
        self.assertEqual(lines[0], "get a = 1")
        self.assertTrue(proc.stdout.splitlines()[-1].startswith("get b = "))

    def test_invalid_lines_ignored(self):
        proc = run_cli("capacity 2\nbogus line here\nset a 1\nget a\n")
        self.assertEqual(proc.stdout.splitlines(), ["get a = 1"])

    def test_ttl_zero_yields_error_line_not_crash(self):
        proc = run_cli("capacity 2\nset a 1 0\nget a\nlen\n")
        self.assertEqual(proc.returncode, 0)
        lines = proc.stdout.splitlines()
        self.assertTrue(any(ln.startswith("ERROR:") for ln in lines))
        self.assertIn("len = 0", lines)

    def test_missing_capacity_emits_error(self):
        proc = run_cli("set a 1\n")
        self.assertEqual(proc.returncode, 0)
        self.assertTrue(proc.stdout.strip().startswith("ERROR:"))


if __name__ == "__main__":
    unittest.main()
