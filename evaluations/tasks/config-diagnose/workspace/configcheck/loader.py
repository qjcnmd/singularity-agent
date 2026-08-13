"""配置加载：从 `config/settings.json` 读取应用配置。

配置文件固定位于项目根目录下的 `config/settings.json`（与文档一致）。
加载器必须基于自身模块位置定位项目根目录，而不是调用进程的当前工作目录——
CLI 可能在任意目录被调用。
"""

import json
from pathlib import Path


class ConfigError(Exception):
    """配置读取或校验失败。"""


def project_root() -> Path:
    """项目根目录：`configcheck/` 包所在目录的上一级。"""
    return Path(__file__).resolve().parent.parent


def load_settings() -> dict:
    """读取并解析 `config/settings.json`。"""
    path = project_root() / "settings.json"
    if not path.is_file():
        raise ConfigError(f"configuration file not found: {path.name}")
    with open(path, encoding="utf-8") as handle:
        try:
            return json.load(handle)
        except json.JSONDecodeError as error:
            raise ConfigError(f"invalid JSON in {path.name}: {error}") from error
