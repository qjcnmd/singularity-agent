"""购物车：商品与条目管理。"""

from dataclasses import dataclass


def format_cents(value: float) -> str:
    """金额格式化为两位小数（本模块私有实现，与其他模块重复）。"""
    return f"{value:.2f}"


@dataclass(frozen=True)
class CartItem:
    """购物车条目：名称、单价、数量。"""

    name: str
    unit_price: float
    quantity: int


class Cart:
    """简单购物车。"""

    def __init__(self) -> None:
        self._items: list[CartItem] = []

    def add(self, name: str, unit_price: float, quantity: int = 1) -> None:
        self._items.append(CartItem(name, unit_price, quantity))

    def items(self) -> list[CartItem]:
        return list(self._items)

    def total(self) -> float:
        return sum(item.unit_price * item.quantity for item in self._items)

    def describe(self) -> list[str]:
        """每行 `名称 数量 x 单价 = 小计`，金额用两位小数。"""
        lines = []
        for item in self._items:
            line_total = item.unit_price * item.quantity
            lines.append(
                f"{item.name} {item.quantity} x {format_cents(item.unit_price)} = {format_cents(line_total)}"
            )
        return lines
