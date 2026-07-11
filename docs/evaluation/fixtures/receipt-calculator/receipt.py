from decimal import Decimal, ROUND_HALF_UP

from pricing import subtotal


_CENT = Decimal("0.01")


def build_receipt(items, tax_rate):
    """Build a decimal receipt from line items and a fractional tax rate."""
    subtotal_amount = subtotal(items)
    tax = (subtotal_amount * Decimal(str(tax_rate))).quantize(
        _CENT, rounding=ROUND_HALF_UP
    )
    return {
        "subtotal": subtotal_amount,
        "tax": tax,
        "total": subtotal_amount,
    }
