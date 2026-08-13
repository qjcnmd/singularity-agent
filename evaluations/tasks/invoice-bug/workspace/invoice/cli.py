"""发票文本渲染与命令行入口。"""

import argparse
import sys

from .pricing import LineItem, discount_rate, discounted_total, subtotal
from .tax import apply_tax


def render_invoice(items: list[LineItem], category: str = "standard") -> str:
    """渲染整张发票文本（小计、折扣、税费、合计）。"""
    lines = []
    for item in items:
        lines.append(
            f"{item.name:<20} {item.quantity:>3} x {item.unit_price:>8.2f} = {item.unit_price * item.quantity:>10.2f}"
        )
    sub = subtotal(items)
    rate = discount_rate(sub)
    after_discount = discounted_total(sub)
    tax = apply_tax(after_discount, category)
    total = after_discount + tax
    lines.append("-" * 46)
    lines.append(f"subtotal:            {sub:>10.2f}")
    if rate < 1.0:
        lines.append(f"discount ({rate:.0%}):   -{sub - after_discount:>10.2f}")
    lines.append(f"tax ({category}):       {tax:>10.2f}")
    lines.append(f"total:               {total:>10.2f}")
    return "\n".join(lines)


def parse_items(args: list[str]) -> list[LineItem]:
    """解析 `名称 单价 数量` 三元组列表。"""
    if len(args) % 3 != 0:
        raise SystemExit("usage: invoice.cli NAME PRICE QTY [NAME PRICE QTY ...]")
    items = []
    for index in range(0, len(args), 3):
        name = args[index]
        price = float(args[index + 1])
        qty = int(args[index + 2])
        items.append(LineItem(name=name, unit_price=price, quantity=qty))
    return items


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="invoice.cli")
    parser.add_argument("--category", default="standard", help="tax category")
    parser.add_argument("entries", nargs="*", help="NAME PRICE QTY triples")
    args = parser.parse_args(argv)
    items = parse_items(args.entries)
    if not items:
        parser.error("at least one NAME PRICE QTY triple is required")
    print(render_invoice(items, args.category))
    return 0


if __name__ == "__main__":
    sys.exit(main())
