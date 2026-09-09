# Domain: finance-purchases

Itemized purchases — what was actually bought, line by line. This is the
**enrichment** layer that turns an opaque bank row (`AMZN $63.07`) into real
spending knowledge: which items, what prices, which category. It is distinct
from the canonical `finance/` ledger, which holds bank/card transactions and
is never touched here — an order and its `AMZN` charge stay separate records
joined at read time. Amazon order-history imports, Apple App Store / iTunes
exports, and (wherever a path exists) grocery-loyalty and email-receipt
parses all write this shape; each source writes its own subfolder and readers
merge them into one purchase history.

This is **privacy-sensitive** (itemized purchase detail — the most detailed
spending data that exists about a person); collectors here ship opt-in with
explicit acknowledgement.

- **Layout:** `finance/purchases/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/finance-purchases.line-item.schema.json`](../schemas/finance-purchases.line-item.schema.json)
- **Dedupe key:** `guid` (source-unique: Amazon `OrderID:lineIndex`, Apple's
  order/transaction id, a receipt id + line index). Imports must skip
  already-stored guids so re-dropped exports are idempotent.

## Granularity: one record per line item

A record is **one purchased line item**, not one order. Itemization is the
entire point — "which 31 items" rather than "$84.12 KROGER" — so the
contract's grain is the item. An **order** is a read-time grouping: every
line of one order shares an `order_id`, and the reader sums them. This
matches the sources directly: Amazon's order-history CSV emits one row per
item; Apple's export is one row per app/media/IAP purchase; an itemized
receipt is a list of items under one transaction. Order-level totals
(shipping, tax, the grand total) are read-time aggregates over the lines plus
the per-line `extra`, never a separate order record.

## Line item

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local order/purchase date (sources give a date; write local midnight, e.g. `2026-05-28T00:00:00-07:00`) |
| `source` | string | ✔ | collector id, = the folder name (`amazon`, `apple-app-store`, a retailer id) |
| `guid` | string | ✔ | source-unique id, the dedupe key (order id + line index, transaction id, …) |
| `merchant` | string | ✔ | where it was bought (`"Amazon"`, the Apple storefront, the retailer) |
| `order_id` | string | | groups the line items of one order/receipt; the read-time grouping key |
| `item` | string | | item description / title (the app name, product title, grocery item) |
| `qty` | number | | quantity purchased |
| `unit_price` | number | | price per unit, in `currency` |
| `amount` | number | | line total (unit_price × qty, before order-level shipping/tax), in `currency` |
| `currency` | string | | ISO 4217 code (`"USD"`); omit when the source doesn't state it |
| `category` | string | | source-native category/department (`"Electronics"`, `"Productivity"`) |
| `url` | string | | product/item link, when the source gives one |
| `status` | string | | source-native fulfilment state (`"delivered"`, `"shipped"`, `"refunded"`, `"canceled"`) |
| `shipment` | object | | shipping detail when present (`{tracking, carrier, delivered, …}`) |
| `extra` | object | | everything source-specific (ASIN, tax, payment instrument, storefront, subscription flag, loyalty id, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-05-28T00:00:00-07:00","source":"amazon","guid":"112-7654321-0011223:1","order_id":"112-7654321-0011223","merchant":"Amazon","item":"Anker USB-C Charger 65W","qty":2,"unit_price":35.99,"amount":71.98,"currency":"USD","category":"Electronics","url":"https://www.amazon.com/gp/product/B08D6T6N9C","status":"delivered","shipment":{"tracking":"TBA303012345678","carrier":"AMZL","delivered":"2026-05-30T15:42:00-07:00"},"extra":{"asin":"B08D6T6N9C","unit_price_tax":3.06,"payment":"Visa ****1234"}}
{"ts":"2026-06-01T00:00:00-07:00","source":"apple-app-store","guid":"MQR4N8KP72","merchant":"App Store","item":"Things 3","amount":49.99,"currency":"USD","category":"Productivity","extra":{"storefront":"US"}}
{"ts":"2026-06-09T00:00:00-07:00","source":"kroger","guid":"recpt-20260609-4471:08","order_id":"recpt-20260609-4471","merchant":"Kroger","item":"Organic Bananas","qty":3,"amount":1.74,"currency":"USD"}
```

## Read-time semantics (FYI for writers)

The purchase reader scans `finance/purchases/*/` and groups by `order_id`
within a source to reconstruct orders; creating your source folder is the
registration. Spending views sum `amount` over lines and join purchases to
the `finance/` ledger by merchant + date window — that join is read-time and
lives in the reader, never baked into a row. Write what the export gave you:
a sparse source (an early Amazon import before tracking arrives, an
itemized-but-priceless loyalty dump) writes a minimal line, and the missing
fields are honest absences, not zeros. Full source fidelity always survives
in the source's own raw rows regardless of what the contract normalizes.
