# 任务：重构购物车模块（提取重复逻辑，保持行为不变）

`shopping` 是一个购物车 Python 包，三个模块各自实现了一套"金额格式化"与"折扣阈值"逻辑：

- `shopping/cart.py`：购物车管理
- `shopping/discounts.py`：折扣计算
- `shopping/receipt.py`：收据渲染

当前代码可以工作（现有测试全绿），但存在重复：三个模块里各自定义了格式化为两位小数的函数（`format_cents`/`fmt_money`/`money_str`），且折扣阈值 `100`/`500` 硬编码出现在多处。

## 要求

按以下约定重构（**不改变任何外部行为**）：

1. 新建 `shopping/money.py`：集中提供
   - `format_amount(value: float) -> str`：格式化为两位小数金额字符串（如 `"19.90"`），由 `cart.py`、`discounts.py`、`receipt.py` 复用，删除三处各自实现；
   - `DISCOUNT_THRESHOLDS: tuple[tuple[float, float], ...]`：折扣阈值常量（`(500, 0.9)` 与 `(100, 0.95)`），由 `discounts.py` 引用，删除散落硬编码。
2. 模块公共导入面不变：`shopping/__init__.py` 的导出、三个模块的公开函数名与签名保持原样。
3. 测试全部通过（`python -m unittest discover -s tests`）。

## 验收标准

- 全部测试通过。
- `shopping/money.py` 存在且导出 `format_amount` 与 `DISCOUNT_THRESHOLDS`。
- `cart.py`、`discounts.py`、`receipt.py` 不再各自定义金额格式化函数（复用 `money.format_amount`）。
- 行为与重构前一致（测试验证）。


## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `invoice/pricing.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。
