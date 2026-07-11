export function grandTotal(lines) {
  if (lines.length === 0) return 0;
  return lines[0].unitPrice * lines[0].quantity;
}
