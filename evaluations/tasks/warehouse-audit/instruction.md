# 任务：修复库存审计工具中的报表错误

`warehouse` 是一个库存审计小工具。它从 CSV 台账（入库/出库流水）出发，经过整条数据流——加载台账、套用出入库方向、按 SKU 汇总库存、按日期梳理变动历史——最后输出库存报表。项目自带单元测试（`tests/`），当前**部分测试失败**。

## 背景

仓库的 `data/ledger.csv` 里是一份示例台账，包含多条入库（`IN`）与出库（`OUT`）记录。运行：

```
python -m warehouse.cli data/ledger.csv
```

应当先打印各 SKU 的**库存结余汇总**（`stock:` 段），再打印按时间正序排列的**出入库变动历史**（`history:` 段）。但当前程序**无法正常运行**：要么崩溃报错，要么输出的库存数字/日期顺序与台账对不上。

请完成以下工作：

1. **先运行测试复现失败**（`python -m unittest discover -s tests -v`），确认哪些测试挂掉。
2. **通读整条数据流**：加载（`warehouse/cli.py`）→ 台账方向与库存（`warehouse/ledger.py`）→ 报表与日期（`warehouse/report.py`）→ 数据模型（`warehouse/models.py`）。
3. 定位并修复**全部**缺陷。注意缺陷可能不止一处，且彼此独立——即使某个缺陷修好了，其它缺陷仍会让运行失败或数据错误。
4. **CSV 表头列顺序不保证固定**：不同台账文件的列顺序可能不同（例如 `sku,kind,qty,...` 与 `name,sku,date,kind,qty,...`）。加载逻辑必须**按表头名称读取字段**（如 `csv.DictReader`），禁止按固定位置索引（`row[0]`/`row[1]` 这类硬编码位置）——按位置读取只在恰好匹配某个文件时可用，换一个列顺序的文件就会读错或崩溃。
5. 保持命令行用法与输出格式不变（`python -m warehouse.cli <csv>`，包含 `stock:` 与 `history:` 两段）。
6. **新增** `tests/test_regression.py`：用 unittest 为修复过程中发现的缺陷各写一个回归测试，确保不会再犯。

## 验收标准

- `python -m unittest discover -s tests` 全部通过（0 failures, 0 errors）。
- `python -m warehouse.cli data/ledger.csv` 能正常退出 0，并按台账正确输出库存结余与按时序排列的历史。
- 对**列顺序不同**的台账文件（自行构造）同样能正确输出——这是按表头名读取的验证。
- `tests/test_regression.py` 存在，且至少覆盖两个缺陷。
- 修复只针对根因，不做无关改动。

## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `warehouse/ledger.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
