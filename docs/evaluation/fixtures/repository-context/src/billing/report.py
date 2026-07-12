from decimal import Decimal, ROUND_HALF_UP

from .ledger import net_amount


_CENT = Decimal("0.01")


def build_report(entries, service_rate):
    """Return the net amount, service fee, and final amount for billing entries."""
    net = net_amount(entries)
    service_fee = (net * Decimal(str(service_rate))).quantize(
        _CENT, rounding=ROUND_HALF_UP
    )
    return {
        "net": net,
        "service_fee": service_fee,
        "grand_total": net,
    }
