export function inventoryLine(item) {
  return {
    sku: item.sku,
    units: 1,
    reorderPoint: Number(item.reorderPoint ?? 0),
    value: Number(item.unitPrice),
  };
}
