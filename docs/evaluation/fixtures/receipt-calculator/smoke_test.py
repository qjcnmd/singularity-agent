import unittest
from decimal import Decimal

from pricing import subtotal
from receipt import build_receipt


class ReceiptSmokeTests(unittest.TestCase):
    def test_multi_line_receipt(self):
        items = [("2.50", 2), ("1.25", 3)]
        self.assertEqual(subtotal(items), Decimal("8.75"))
        self.assertEqual(
            build_receipt(items, "0.10"),
            {
                "subtotal": Decimal("8.75"),
                "tax": Decimal("0.88"),
                "total": Decimal("9.63"),
            },
        )


if __name__ == "__main__":
    unittest.main()
