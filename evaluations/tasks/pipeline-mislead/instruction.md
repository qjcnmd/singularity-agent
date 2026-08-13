# 任务：修复数据管道中的键缺失错误

`pipeline` 是一个处理 CSV 销售数据的小工具：加载数据 → 按日期聚合 → 生成报表。

## 现象

运行 `python -m pipeline.cli data/sales.csv` 时程序崩溃：

```
Traceback (most recent call last):
  ...
  File "pipeline\aggregate.py", line 13, in aggregate_by_date
    date = record["date"]
KeyError: 'date'
```

错误发生在聚合阶段，看起来像是聚合代码漏了字段。但聚合逻辑本身没有问题——真实根因在更早的处理阶段。请阅读**整条数据流**（加载 → 解析 → 聚合 → 报表），找到真实根因并修复。

数据文件第一行是表头：`date,product,count`。修复后 `python -m pipeline.cli data/sales.csv` 应正常输出每行 `日期 总数`，且 `tests/` 全部通过。

## 验收标准

- `python -m unittest discover -s tests` 全部通过。
- `python -m pipeline.cli data/sales.csv` 正常输出（退出码 0），每行格式 `YYYY-MM-DD <count>`。
- 修复的是真实根因（输入解析阶段），不是用 `dict.get()` 掩盖键缺失。


## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `invoice/pricing.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
