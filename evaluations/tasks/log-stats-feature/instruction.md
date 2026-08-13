# 任务：为日志统计工具实现按小时聚合功能

`logstats` 是一个解析日志文件并输出统计信息的命令行工具。它目前支持按日志级别（INFO/WARN/ERROR）统计条数。

项目附带测试（`tests/`）。现有功能对应的测试全部通过，但**新测试 `test_hourly.py` 失败**：它定义了一个尚未实现的功能——按小时聚合。

## 需求

为 CLI 新增 `--hourly` 选项：按日志时间戳的**小时**分组，输出每小时各级别条数（以及每小时总数），时间桶格式为 `YYYY-MM-DD HH:00`。具体行为以 `tests/test_hourly.py` 的断言为准。

日志行格式：`2026-08-14 09:15:30 INFO message text`（ISO 时间戳 + 级别 + 消息，单空格分隔）。缺失或格式错误的行应被跳过（不崩溃）。

## 验收标准

- `python -m unittest discover -s tests` 全部通过。
- 现有行为（按级别统计、无参数默认输出）不变。
- 命令行用法：`python -m logstats.cli --hourly <logfile>`。


## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `invoice/pricing.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
