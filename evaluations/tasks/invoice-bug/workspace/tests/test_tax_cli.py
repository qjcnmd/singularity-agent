"""税费与发票渲染测试。"""

import unittest

from invoice.cli import parse_items, render_invoice
from invoice.pricing import LineItem
from invoice.tax import apply_tax, tax_rate


class TaxRateTest(unittest.TestCase):
    def test_standard_and_reduced_rates(self):
        self.assertEqual(tax_rate("standard"), 6.0)
        self.assertEqual(tax_rate("essential"), 3.0)
        self.assertEqual(tax_rate("unknown"), 6.0)

    def test_apply_tax_rounds_to_cents(self):
        self.assertEqual(apply_tax(100.0), 106.0)
        self.assertEqual(apply_tax(100.0, "essential"), 103.0)
        self.assertEqual(apply_tax(0.0), 0.0)

    def test_apply_tax_integer_amounts(self):
        self.assertEqual(apply_tax(475.0), 503.5)
        self.assertEqual(apply_tax(900.0, "essential"), 927.0)


class CliTest(unittest.TestCase):
    def test_parse_items_triples(self):
        items = parse_items(["lamp", "19.99", "2", "book", "12.5", "3"])
        self.assertEqual(items, [LineItem("lamp", 19.99, 2), LineItem("book", 12.5, 3)])

    def test_render_invoice_contains_key_lines(self):
        text = render_invoice([LineItem("lamp", 100.0, 1)])
        self.assertIn("subtotal:", text)
        self.assertIn("tax (standard):", text)
        self.assertIn("total:", text)
        self.assertIn("106.00", text)

    def test_render_invoice_applies_discount_line(self):
        text = render_invoice([LineItem("desk", 1000.0, 1)])
        self.assertIn("discount (90%):", text)
        self.assertIn("100.00", text)
        self.assertIn("954.00", text)  # 折扣后小计 900.00 × 6% 税


if __name__ == "__main__":
    unittest.main()
