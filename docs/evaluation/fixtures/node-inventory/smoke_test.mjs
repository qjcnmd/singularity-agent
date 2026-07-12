import assert from "node:assert/strict";
import { summarizeInventory } from "./inventory.mjs";

const result = summarizeInventory([
  { sku: "A-17", unitPrice: 4.5, quantity: 2, reorderPoint: 3 },
  { sku: "B-04", unitPrice: 3, quantity: 5, reorderPoint: 4 },
]);

assert.deepEqual(result, {
  totalUnits: 7,
  totalValue: 24,
  lowStockSkus: ["A-17"],
});
