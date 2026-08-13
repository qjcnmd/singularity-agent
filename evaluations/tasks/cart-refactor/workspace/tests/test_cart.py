"""购物车行为测试：重构后必须保持全绿。"""

import unittest

from shopping import Cart, CartItem, apply_discount, discount_for, render_receipt


class CartTest(unittest.TestCase):
    def test_add_and_total(self):
        cart = Cart()
        cart.add("book", 19.99, 2)
        cart.add("pen", 1.25)
        self.assertEqual(cart.total(), 41.23)

    def test_describe_formats_amounts(self):
        cart = Cart()
        cart.add("book", 19.99, 2)
        lines = cart.describe()
        self.assertEqual(lines[0], "book 2 x 19.99 = 39.98")

    def test_items_returns_copies(self):
        cart = Cart()
        cart.add("cup", 5.0)
        items = cart.items()
        self.assertEqual(items, [CartItem("cup", 5.0, 1)])


class DiscountTest(unittest.TestCase):
    def test_thresholds(self):
        self.assertEqual(discount_for(50.0), 1.0)
        self.assertEqual(discount_for(100.0), 0.95)
        self.assertEqual(discount_for(499.99), 0.95)
        self.assertEqual(discount_for(500.0), 0.9)
        self.assertEqual(discount_for(1000.0), 0.9)

    def test_apply_discount(self):
        self.assertEqual(apply_discount(100.0), 95.0)
        self.assertEqual(apply_discount(500.0), 450.0)
        self.assertEqual(apply_discount(50.0), 50.0)

    def test_discount_line_text(self):
        from shopping.discounts import discount_line

        self.assertEqual(discount_line(50.0), "no discount")
        self.assertEqual(discount_line(100.0), "discount 5% (5.00)")
        # 现有行为：int((1 - 0.9) * 100) 的浮点表示显示为 9%。
        self.assertEqual(discount_line(500.0), "discount 9% (50.00)")


class ReceiptTest(unittest.TestCase):
    def test_receipt_lines(self):
        text = render_receipt(100.0)
        lines = text.splitlines()
        self.assertEqual(lines[0], "subtotal: 100.00")
        self.assertEqual(lines[1], "discount: 5.00")
        self.assertEqual(lines[2], "tax: 5.70")
        self.assertEqual(lines[3], "total: 100.70")

    def test_receipt_no_discount_below_threshold(self):
        text = render_receipt(50.0)
        lines = text.splitlines()
        self.assertEqual(lines[1], "discount: 0.00")


if __name__ == "__main__":
    unittest.main()
