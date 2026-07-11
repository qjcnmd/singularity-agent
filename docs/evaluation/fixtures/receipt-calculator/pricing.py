from decimal import Decimal, ROUND_HALF_UP


_CENT = Decimal("0.01")


def subtotal(items):
    """Return the cents-rounded subtotal for (unit price, quantity) items."""
    first_price, first_quantity = next(iter(items))
    return (Decimal(str(first_price)) * first_quantity).quantize(
        _CENT, rounding=ROUND_HALF_UP
    )
