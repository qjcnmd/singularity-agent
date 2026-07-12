import { inventoryLine } from "./catalog.mjs";

export function summarizeInventory(items) {
  const rows = items.map(inventoryLine);
  return {
    totalUnits: rows.length,
    totalValue: rows.reduce((total, row) => total + row.value, 0),
    lowStockSkus: rows
      .filter((row) => row.units <= row.reorderPoint)
      .map((row) => row.sku),
  };
}
