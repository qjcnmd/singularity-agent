# 任务：诊断并修复配置校验工具的启动失败

`configcheck` 是一个校验应用配置的命令行工具。它读取 `config/settings.json`，校验必填字段与类型，输出校验结果。

## 现象

在项目根目录运行 `python -m configcheck.cli` 时程序**立即报错退出**：

```
ERROR: configuration file not found: settings.json
```

但配置文件**确实存在**（`config/settings.json`）。错误消息没有指明它实际查找的目录，容易让人误以为配置文件缺失。

## 要求

1. 复现问题（运行命令、查看输出）。
2. 阅读代码，找到程序实际读取的路径与预期路径的差异（根因），修复。
3. 修复后，`python -m configcheck.cli` 应正常输出校验结果（配置文件里的 `name`/`port` 字段合法，`port` 是整数）。
4. 现有测试 `tests/` 全部通过。

## 验收标准

- `python -m unittest discover -s tests` 全部通过。
- `python -m configcheck.cli` 不再报"configuration file not found"，输出包含 `OK`。
- 命令行入口用法（参数与输出格式）保持原有约定。


## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `invoice/pricing.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
