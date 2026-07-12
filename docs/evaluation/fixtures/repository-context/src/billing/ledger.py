from decimal import Decimal, ROUND_HALF_UP


_CENT = Decimal("0.01")


def net_amount(entries):
    """Return the cent-rounded sum of signed billing entry amounts."""
    total = Decimal(str(entries[0]["amount"])) if entries else Decimal("0.00")
    return total.quantize(
        _CENT, rounding=ROUND_HALF_UP
    )
