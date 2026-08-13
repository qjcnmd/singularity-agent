"""加载器与校验器测试。"""

import json
import os
import subprocess
import sys
import unittest

from configcheck.loader import ConfigError, load_settings, project_root
from configcheck.validator import validate


class ProjectRootTest(unittest.TestCase):
    def test_project_root_is_workspace_root(self):
        root = project_root()
        self.assertTrue((root / "config").is_dir())
        self.assertTrue((root / "configcheck").is_dir())


class LoadSettingsTest(unittest.TestCase):
    def test_loads_existing_config(self):
        settings = load_settings()
        self.assertEqual(settings["name"], "demo-service")
        self.assertEqual(settings["port"], 8080)

    def test_config_lives_in_config_subdirectory(self):
        """配置必须从 `config/settings.json` 读取（文档约定）。"""
        path = project_root() / "config" / "settings.json"
        self.assertTrue(path.is_file(), "config/settings.json must exist")
        with open(path, encoding="utf-8") as handle:
            self.assertEqual(json.load(handle)["port"], 8080)


class ValidateTest(unittest.TestCase):
    def test_valid_settings_have_no_problems(self):
        self.assertEqual(validate({"name": "svc", "port": 8080}), [])

    def test_missing_name_is_a_problem(self):
        problems = validate({"port": 8080})
        self.assertEqual(len(problems), 1)
        self.assertIn("name", problems[0])

    def test_non_integer_port_is_a_problem(self):
        problems = validate({"name": "svc", "port": "8080"})
        self.assertEqual(len(problems), 1)
        self.assertIn("port", problems[0])


class CliTest(unittest.TestCase):
    def run_cli(self):
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        return subprocess.run(
            [sys.executable, "-m", "configcheck.cli"],
            capture_output=True,
            text=True,
            cwd=root,
        )

    def test_cli_exits_zero_with_ok(self):
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("OK", result.stdout)

    def test_cli_error_message_mentions_config_directory(self):
        """错误消息应包含 `config/` 目录段，便于诊断（不能只说文件名）。"""
        result = self.run_cli()
        if result.returncode != 0:
            self.assertIn("config" + os.sep, result.stdout.lower())


if __name__ == "__main__":
    unittest.main()
