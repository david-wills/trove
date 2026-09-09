//! The `finance-purchases` domain contract: itemized purchases — what was
//! actually bought, line by line — in one normalized, source-agnostic store.
//! This is the **enrichment** layer that turns an opaque ledger row
//! (`AMZN $63.07`) into real spending knowledge (which items, what prices,
//! which category); it is distinct from the canonical `finance/` ledger, which
//! holds bank/card/chain transactions and is never touched here. An order and
//! its charge stay separate records joined at read time.
//!
//! One record shape, [`LineItem`], under
//! `finance/purchases/<source>/YYYY-MM.jsonl` (`<source>` is the collector id
//! and the folder name; the month is the month of [`LineItem::ts`]). Amazon
//! order-history imports, Apple App Store / iTunes exports, grocery-loyalty and
//! email-receipt parses, and (as the first collector binding this contract) the
//! Bitcoin wallet's on-chain value transfers all write this shape; each source
//! writes its own subfolder and readers merge them into one purchase history.
//!
//! ## Granularity: one record per line item
//!
//! A record is **one purchased line item**, not one order — itemization is the
//! whole point. An *order* is a read-time grouping: every line of one order
//! shares an `order_id`, and the reader sums them. Order-level totals
//! (shipping, tax, grand total) are read-time aggregates over the lines plus
//! the per-line `extra`, never a separate order record.
//!
//! Only `ts`/`source`/`guid`/`merchant` are required; everything else is
//! omit-empty, so a sparse source (an early Amazon import before tracking
//! arrives, an itemized-but-priceless loyalty dump) writes a minimal line and
//! the missing fields are honest absences, not zeros. Source-specific fields
//! the normalized columns don't carry ride verbatim under `extra` rather than
//! being dropped. This is **privacy-sensitive** (the most detailed spending
//! data that exists about a person); collectors here ship opt-in with explicit
//! acknowledgement.
//!
//! See [`docs/vault-spec/domains/finance-purchases.md`] for the field-level
//! spec; the schema field descriptions there are authoritative for
//! names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One purchased line item — one line of
/// `finance/purchases/<source>/YYYY-MM.jsonl`, grouped into an order at read
/// time by `order_id`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only
/// `ts`/`source`/`guid`/`merchant` are required; everything else is omit-empty.
/// Matches `finance-purchases.line-item.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct LineItem {
    /// RFC3339 local order/purchase date (sources give a date; write local
    /// midnight). Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`amazon`,
    /// `apple-app-store`, `bitcoin`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (order id + line index, transaction id,
    /// a chain txid). Always serialized.
    pub guid: String,
    /// Where it was bought (`"Amazon"`, the Apple storefront, the retailer; for
    /// an on-chain transfer, the network/asset). Always serialized — a purchase
    /// with no merchant is meaningless.
    pub merchant: String,
    /// Groups the line items of one order/receipt; the read-time grouping key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub order_id: String,
    /// Item description / title (the app name, product title, grocery item).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub item: String,
    /// Quantity purchased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qty: Option<f64>,
    /// Price per unit, in `currency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<f64>,
    /// Line total (`unit_price` × `qty`, before order-level shipping/tax), in
    /// `currency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    /// ISO 4217 code (`"USD"`); omit when the source doesn't state it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency: String,
    /// Source-native category/department (`"Electronics"`, `"Productivity"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    /// Product/item link, when the source gives one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Source-native fulfilment state (`"delivered"`, `"shipped"`,
    /// `"refunded"`, `"canceled"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Shipping detail when present (`{tracking, carrier, delivered, …}`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub shipment: Map<String, Value>,
    /// Everything source-specific (ASIN, tax, payment instrument, storefront,
    /// subscription flag, loyalty id, on-chain direction/counterparties, …) —
    /// full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl LineItem {
    /// A minimal record with only the four required fields set.
    pub fn new(
        source: impl Into<String>,
        guid: impl Into<String>,
        ts: impl Into<String>,
        merchant: impl Into<String>,
    ) -> Self {
        LineItem {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            merchant: merchant.into(),
            order_id: String::new(),
            item: String::new(),
            qty: None,
            unit_price: None,
            amount: None,
            currency: String::new(),
            category: String::new(),
            url: String::new(),
            status: String::new(),
            shipment: Map::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_line_item_serializes_only_required_fields() {
        let li = LineItem::new("bitcoin", "abc123:received", "2026-05-28T00:00:00-07:00", "Bitcoin");
        // Omit-empty: a sparse line is exactly the four required keys.
        assert_eq!(
            serde_json::to_value(&li).unwrap(),
            json!({
                "ts": "2026-05-28T00:00:00-07:00",
                "source": "bitcoin",
                "guid": "abc123:received",
                "merchant": "Bitcoin"
            })
        );
    }

    #[test]
    fn full_line_item_round_trips_with_numeric_money() {
        let line = json!({
            "ts": "2026-05-28T00:00:00-07:00",
            "source": "amazon",
            "guid": "112-7654321-0011223:1",
            "order_id": "112-7654321-0011223",
            "merchant": "Amazon",
            "item": "Anker USB-C Charger 65W",
            "qty": 2,
            "unit_price": 35.99,
            "amount": 71.98,
            "currency": "USD",
            "category": "Electronics",
            "url": "https://www.amazon.com/gp/product/B08D6T6N9C",
            "status": "delivered",
            "shipment": {"tracking": "TBA303012345678", "carrier": "AMZL"},
            "extra": {"asin": "B08D6T6N9C", "unit_price_tax": 3.06}
        });
        let li: LineItem = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(li.qty, Some(2.0), "qty is a number");
        assert_eq!(li.unit_price, Some(35.99));
        assert_eq!(li.amount, Some(71.98));
        assert_eq!(li.merchant, "Amazon");
        assert_eq!(li.extra.get("asin"), Some(&json!("B08D6T6N9C")));
        // Round-trip stability: re-serializing and re-parsing yields the same
        // struct. (A whole-number `qty` of 2 serializes as `2.0` — a JSON
        // `number`, which the schema accepts — so we assert struct equality, not
        // byte-identity with the integer literal in `line`.)
        let re: LineItem = serde_json::from_value(serde_json::to_value(&li).unwrap()).unwrap();
        assert_eq!(re, li, "LineItem round-trips through JSON unchanged");
    }

    #[test]
    fn unknown_fields_tolerated_and_fractional_btc_amount_ok() {
        // Forward-compat: an unknown top-level field is ignored. A signed
        // fractional amount (a BTC value transfer) round-trips as a number.
        let line = json!({
            "ts": "2026-06-09T00:00:00-07:00",
            "source": "bitcoin",
            "guid": "f3a9c1b27e:sent",
            "merchant": "Bitcoin",
            "item": "Sent",
            "amount": -0.0125,
            "currency": "BTC",
            "future_field": "ignored",
            "extra": {"direction": "sent", "fee_sats": 4200}
        });
        let li: LineItem = serde_json::from_value(line).unwrap();
        assert_eq!(li.amount, Some(-0.0125), "signed fractional BTC amount");
        assert_eq!(li.currency, "BTC");
        let re = serde_json::to_value(&li).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("order_id").is_none(), "empty order_id omitted");
    }
}
