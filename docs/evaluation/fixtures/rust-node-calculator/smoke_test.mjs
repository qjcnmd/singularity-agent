import assert from "node:assert/strict";
import { grandTotal } from "./calculator.mjs";

assert.equal(
  grandTotal([
    { unitPrice: 7, quantity: 3 },
    { unitPrice: 5, quantity: 2 },
  ]),
  31,
);
