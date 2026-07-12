import unittest
from decimal import Decimal

from src.billing.ledger import net_amount
from src.billing.report import build_report


class BillingSmokeTests(unittest.TestCase):
    def test_report_aggregates_sales_and_refunds(self):
        entries = [
            {"reference": "sale-100", "amount": "125.50"},
            {"reference": "refund-20", "amount": "-20.00"},
        ]
        self.assertEqual(net_amount(entries), Decimal("105.50"))
        self.assertEqual(
            build_report(entries, "0.05"),
            {
                "net": Decimal("105.50"),
                "service_fee": Decimal("5.28"),
                "grand_total": Decimal("110.78"),
            },
        )


if __name__ == "__main__":
    unittest.main()
