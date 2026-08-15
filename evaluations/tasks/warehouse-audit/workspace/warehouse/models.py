"""库存审计的数据模型。

- StockItem：某 SKU 的静态信息（名称、单价）。
- LedgerEntry：台账中的一条出入库记录。
"""

from dataclasses import dataclass


@dataclass
class StockItem:
    """某 SKU 的静态信息。"""

    sku: str
    name: str
    unit_price: float


@dataclass
class LedgerEntry:
    """台账中的一条记录。

    ``kind`` 为方向：``"IN"`` 表示入库，``"OUT"`` 表示出库。
    ``qty`` 为数量（始终为正数，方向由 ``kind`` 决定）。
    """

    sku: str
    qty: int
    kind: str
