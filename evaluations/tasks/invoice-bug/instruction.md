# 任务：修复发票计算工具中的金额错误

`invoice` 是一个生成发票的 Python 小工具。它从命令行读取商品条目，计算小计、折扣与税费，输出发票文本。

## 背景

项目自带单元测试（`tests/`）。当前**部分测试失败**：发票金额的税费计算在特定金额上出现 1 分钱的误差。

请：

1. 先运行测试复现失败（`python -m unittest discover -s tests -v`）。
2. 阅读代码，定位根因并修复。
3. 确保**全部测试通过**，且不破坏现有命令行行为（`python -m invoice.cli` 用法不变）。

## 验收标准

- `python -m unittest discover -s tests` 全部通过（0 failures, 0 errors）。
- 修复只针对根因，不做无关改动。
- 命令行入口 `python -m invoice.cli` 仍可用（参数与输出格式不变）。


## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `invoice/pricing.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
