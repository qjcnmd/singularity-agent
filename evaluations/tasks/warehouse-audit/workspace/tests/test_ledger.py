"""台账处理测试：出入库方向与净变动。"""

import unittest

from warehouse.ledger import current_stock, entry_sign, net_change
from warehouse.models import LedgerEntry


class EntrySignTest(unittest.TestCase):
    def test_in_increases_stock(self):
        self.assertEqual(entry_sign(LedgerEntry("A", 10, "IN")), 1)

    def test_out_decreases_stock(self):
        self.assertEqual(entry_sign(LedgerEntry("A", 3, "OUT")), -1)


class CurrentStockTest(unittest.TestCase):
    def test_only_in(self):
        entries = [LedgerEntry("A", 10, "IN")]
        self.assertEqual(current_stock(entries), {"A": 1010})

    def test_out_after_in(self):
        entries = [
            LedgerEntry("A", 10, "IN"),
            LedgerEntry("A", 3, "OUT"),
        ]
        # 入库 +10、出库 -3，结余应为 1000 + 10 - 3 = 1007。
        self.assertEqual(current_stock(entries), {"A": 1007})

    def test_empty(self):
        self.assertEqual(current_stock([]), {})


class NetChangeTest(unittest.TestCase):
    def test_net_change_accumulates(self):
        entries = [
            LedgerEntry("A", 10, "IN"),
            LedgerEntry("A", 3, "OUT"),
            LedgerEntry("A", 2, "IN"),
        ]
        self.assertEqual(net_change(entries), {"A": 9})


if __name__ == "__main__":
    unittest.main()
