//! Amazon Order History — itemized purchase records from the Privacy Central
//! data-request ZIP export.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/amazon.md.
//!
//! ## Access
//!
//! Official path: amazon.com → Account → Privacy Central → Request Your Data
//! → Order History → email notification → download ZIP (takes hours to days).
//! The ZIP contains `Retail.OrderHistory.1/Retail.OrderHistory.1.csv` (canonical
//! subfolder; Amazon also ships a `.2` variant). No API, no credential held
//! by Trove; standalone-clean.
//!
//! ## CSV shape (confirmed against two real Privacy Central exports, 2023 + 2025)
//!
//! Fields (all quoted, BOM-prefixed on the first sample):
//!   Website, Order ID, Order Date, Purchase Order Number, Currency,
//!   Unit Price, Unit Price Tax, Shipping Charge, Total Discounts, Total Owed,
//!   Shipment Item Subtotal, Shipment Item Subtotal Tax, ASIN, Product Condition,
//!   Quantity, Payment Instrument Type, Order Status, Shipment Status, Ship Date,
//!   Shipping Option, Shipping Address, Billing Address,
//!   Carrier Name & Tracking Number, Product Name,
//!   Gift Message, Gift Sender Name, Gift Recipient Contact Details
//!
//! A later export (2025) adds "Item Serial Number" at the end; the name-based
//! parser is robust to the absence or presence of any optional trailing column.
//!
//! ## Mapping → `finance-purchases` contract
//!
//! One [`crate::finance::LineItem`] per CSV row (one purchased line item):
//!   - `ts`        = "Order Date" (ISO-8601 UTC; rendered as RFC3339 local local-midnight equivalent)
//!   - `source`    = `"amazon"`
//!   - `guid`      = Order ID + `:` + per-order line index (one order has many rows)
//!   - `merchant`  = "Amazon" (or the "Website" column when it differs from "Amazon.com")
//!   - `order_id`  = "Order ID"
//!   - `item`      = "Product Name"
//!   - `qty`       = "Quantity" (integer, as f64)
//!   - `unit_price` = "Unit Price" (f64)
//!   - `amount`    = "Shipment Item Subtotal" (per-line pre-tax total = qty × unit_price, f64)
//!   - `currency`  = "Currency" (typically "USD")
//!   - `status`    = "Order Status" lowercased ("closed", "cancelled", etc.)
//!   - `shipment`  = `{tracking, carrier_raw, ship_date, shipment_status, shipping_option}` when present
//!   - `extra`     = ASIN, unit_price_tax, shipping_charge, total_discounts, total_owed,
//!                   shipment_item_subtotal_tax,
//!                   product_condition, payment_instrument_type, website (if non-standard)
//!
//! ## Two layers
//!
//! - **raw** — verbatim row (`serde_json::Value`) at full fidelity under
//!   `finance/purchases/amazon/raw/YYYY-MM.jsonl`, partitioned by order month.
//! - **contract** — normalized [`crate::finance::LineItem`] rows under
//!   `finance/purchases/amazon/YYYY-MM.jsonl`.
//!
//! Re-importing the same (or a newer) export is idempotent — guid dedupe prevents
//! duplicates. The `guid` format (`<OrderID>:<line_index>`) is stable as long as
//! Amazon's export preserves per-order line ordering, which has been observed to be
//! consistent across re-exports. Fragility note: if Amazon were to split or reorder
//! shipment lines in a future export, the positional index could mis-identify items;
//! a key of `<OrderID>:<ASIN>:<occurrence>` would be more robust but requires handling
//! repeated ASINs within the same order.
//!
//! Billing address and gift metadata never ride the contract layer (privacy
//! minimization); they go into raw only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{Map, Value};

use crate::finance::LineItem;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer path (source subfolder).
const DIR: &str = "finance/purchases/amazon";
/// Raw-layer path — full-fidelity verbatim rows.
const RAW_DIR: &str = "finance/purchases/amazon/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "amazon",
        name: "Amazon Orders",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Amazon order history from the Privacy Central data export \
                      (Retail.OrderHistory.1 CSV or ZIP). Enriches your spending history with \
                      line-item detail: what was bought, the ASIN, price, category, and tracking. \
                      Re-runnable: re-importing a newer export never duplicates.",
        domain: "finance",
        vault_path: "finance/purchases/amazon/",
        toggleable: false,
        setup: &[
            "amazon.com → Account → Privacy Central → Request Your Data → Order History.",
            "Amazon sends an email when the export is ready — this can take hours to days.",
            "Drop the downloaded ZIP here, or the CSV from inside it \
             (Retail.OrderHistory.1/Retail.OrderHistory.1.csv).",
        ],
        caveats: "The data request can take hours to days before the download is ready. \
                  This is the official export path — native CSV download was removed in 2023. \
                  This is financial detail (itemized purchases); the integration ships opt-in.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// ZIP / CSV extraction.

/// Read the CSV body from the given path.
///
/// Accepts:
/// - A bare `.csv` (reads it directly).
/// - A `.zip` (Privacy Central export): looks for
///   `Retail.OrderHistory.1/Retail.OrderHistory.1.csv` inside the archive (canonical).
///   Falls back to any entry whose name contains `Retail.OrderHistory` and ends with `.csv`,
///   which covers both the `.1` and `.2` export variants Amazon has shipped as well as
///   renamed ZIPs.
fn order_history_csv(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading ZIP {}", path.display()))?;

        // Canonical path first, then fall back to any Retail.OrderHistory*.csv entry.
        // Amazon has shipped both .1 and .2 variants; match either.
        let canonical = "Retail.OrderHistory.1/Retail.OrderHistory.1.csv";
        let entry_name = if archive.by_name(canonical).is_ok() {
            canonical.to_string()
        } else {
            let found = (0..archive.len())
                .find_map(|i| {
                    let e = archive.by_index(i).ok()?;
                    let name = e.name().to_string();
                    if name.contains("Retail.OrderHistory") && name.ends_with(".csv") {
                        Some(name)
                    } else {
                        None
                    }
                });
            match found {
                Some(n) => n,
                None => anyhow::bail!(
                    "no Retail.OrderHistory*.csv found in the ZIP — \
                     is this an Amazon Privacy Central Order History export?"
                ),
            }
        };

        let mut entry = archive
            .by_name(&entry_name)
            .with_context(|| format!("reading {entry_name} from ZIP"))?;
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body)
            .with_context(|| format!("reading {entry_name}"))?;
        Ok(body)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Raw row: the verbatim CSV row as a JSON object, tagged with `ts` for
// partition routing. On disk the line is the verbatim object (all columns).

#[derive(serde::Serialize)]
struct RawLine {
    /// Same RFC3339 local ts as the contract row — used for month partition.
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Order Date parsing.

/// "2023-11-12T04:20:39Z" (UTC ISO-8601, confirmed against real exports)
/// → RFC3339 local (the vault convention).
///
/// The export also includes fractional seconds on some entries
/// ("2025-11-02T15:41:10.586Z") — `DateTime::parse_from_rfc3339` handles
/// both. Local midnight for the order date (same day in the local timezone).
fn parse_order_date(raw: &str) -> Option<String> {
    let t = raw.trim();
    // Parse as RFC3339 (covers both Z and ±HH:MM, with or without fractional seconds).
    let utc = DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))?;
    // Convert to local and take the date, then write local midnight.
    let local_date = utc.with_timezone(&Local).date_naive();
    let local_midnight = Local
        .from_local_datetime(&local_date.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    Some(local_midnight.to_rfc3339())
}

/// Parse an amount string ("6.69", "0", "'-1.34'", "Not Available") to f64.
/// Returns None for non-numeric / "Not Available".
///
/// Amazon wraps negative discount values in single-quotes inside the CSV
/// double-quote cell: `"'-1.34'"`. Strip inner quotes and parse.
fn parse_amount(raw: &str) -> Option<f64> {
    let t = raw.trim().trim_matches('\'').trim_matches('"');
    if t.is_empty() || t.eq_ignore_ascii_case("not available") {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Extract the tracking number and carrier from "Carrier Name & Tracking Number":
///   "AMZN_US(TBA309740535859)"      → tracking = "TBA309740535859", carrier = "AMZN_US"
///   "UPS(1Z12345E0291980793)"       → tracking = "1Z12345E0291980793", carrier = "UPS"
///   "Not Available"                 → (None, None)
fn parse_tracking(raw: &str) -> (Option<String>, Option<String>) {
    let t = raw.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("not available") {
        return (None, None);
    }
    if let Some(paren) = t.find('(') {
        let carrier = t[..paren].trim().to_string();
        let rest = &t[paren + 1..];
        let tracking = rest.trim_end_matches(')').trim().to_string();
        (
            if tracking.is_empty() { None } else { Some(tracking) },
            if carrier.is_empty() { None } else { Some(carrier) },
        )
    } else {
        // No parens — treat the whole value as a tracking number.
        (Some(t.to_string()), None)
    }
}

/// Build a shipment map from the relevant CSV fields. Returns an empty map
/// when nothing useful is present.
fn make_shipment(
    carrier_raw: &str,
    ship_date: &str,
    shipment_status: &str,
    shipping_option: &str,
) -> Map<String, Value> {
    let mut m = Map::new();
    let (tracking, carrier) = parse_tracking(carrier_raw);
    if let Some(tr) = tracking {
        m.insert("tracking".into(), Value::String(tr));
    }
    if let Some(ca) = carrier {
        m.insert("carrier".into(), Value::String(ca));
    }
    // Keep the raw carrier string for full fidelity.
    let cr = carrier_raw.trim();
    if !cr.is_empty() && !cr.eq_ignore_ascii_case("not available") {
        m.insert("carrier_raw".into(), Value::String(cr.to_string()));
    }
    let sd = ship_date.trim();
    if !sd.is_empty() && !sd.eq_ignore_ascii_case("not available") {
        m.insert("ship_date".into(), Value::String(sd.to_string()));
    }
    let ss = shipment_status.trim();
    if !ss.is_empty() && !ss.eq_ignore_ascii_case("not available") {
        m.insert("shipment_status".into(), Value::String(ss.to_string()));
    }
    let so = shipping_option.trim();
    if !so.is_empty() && !so.eq_ignore_ascii_case("not available") {
        m.insert("shipping_option".into(), Value::String(so.to_string()));
    }
    m
}

/// Normalize "not available" to an empty string for optional text fields.
fn na_to_empty(s: &str) -> String {
    let t = s.trim();
    if t.eq_ignore_ascii_case("not available") { String::new() } else { t.to_string() }
}

// ---------------------------------------------------------------------------
// Column-position map (name-based; robust to extra/missing columns).

struct Cols {
    website: usize,
    order_id: usize,
    order_date: usize,
    currency: usize,
    unit_price: usize,
    unit_price_tax: Option<usize>,
    shipping_charge: Option<usize>,
    total_discounts: Option<usize>,
    total_owed: Option<usize>,
    shipment_item_subtotal: Option<usize>,
    shipment_item_subtotal_tax: Option<usize>,
    asin: usize,
    product_condition: Option<usize>,
    quantity: usize,
    payment_instrument_type: Option<usize>,
    order_status: usize,
    shipment_status: Option<usize>,
    ship_date: Option<usize>,
    shipping_option: Option<usize>,
    carrier_tracking: Option<usize>,
    product_name: usize,
    // Shipping/billing addresses and gift fields go raw-only (tracked for mapped-set, never read).
    #[allow(dead_code)]
    shipping_address: Option<usize>,
    #[allow(dead_code)]
    billing_address: Option<usize>,
    #[allow(dead_code)]
    gift_message: Option<usize>,
    #[allow(dead_code)]
    gift_sender_name: Option<usize>,
    #[allow(dead_code)]
    gift_recipient: Option<usize>,
    item_serial_number: Option<usize>,
    /// All column indices above (to route unmapped columns into raw).
    mapped: HashSet<usize>,
}

fn norm_header(h: &str) -> String {
    h.trim_start_matches('\u{feff}').trim().to_ascii_lowercase()
}

fn detect_cols(headers: &[String]) -> Result<Cols> {
    let h: Vec<String> = headers.iter().map(|s| norm_header(s)).collect();
    let idx = |name: &str| h.iter().position(|c| c == name);
    let req = |name: &str| -> Result<usize> {
        idx(name).with_context(|| {
            format!(
                "missing required column \"{name}\" — is this an Amazon Retail.OrderHistory.1 CSV? \
                 Found columns: {}", h.join(", ")
            )
        })
    };

    let website = req("website")?;
    let order_id = req("order id")?;
    let order_date = req("order date")?;
    let currency = req("currency")?;
    let unit_price = req("unit price")?;
    let product_name = req("product name")?;
    let quantity = req("quantity")?;
    let order_status = req("order status")?;
    let asin = req("asin")?;

    let unit_price_tax = idx("unit price tax");
    let shipping_charge = idx("shipping charge");
    let total_discounts = idx("total discounts");
    let total_owed = idx("total owed");
    let shipment_item_subtotal = idx("shipment item subtotal");
    let shipment_item_subtotal_tax = idx("shipment item subtotal tax");
    let product_condition = idx("product condition");
    let payment_instrument_type = idx("payment instrument type");
    let shipment_status = idx("shipment status");
    let ship_date = idx("ship date");
    let shipping_option = idx("shipping option");
    let carrier_tracking = idx("carrier name & tracking number");
    let shipping_address = idx("shipping address");
    let billing_address = idx("billing address");
    let gift_message = idx("gift message");
    let gift_sender_name = idx("gift sender name");
    let gift_recipient = idx("gift recipient contact details");
    let item_serial_number = idx("item serial number");

    let mut mapped: HashSet<usize> = HashSet::new();
    for i in [
        Some(website), Some(order_id), Some(order_date), Some(currency),
        Some(unit_price), Some(product_name), Some(quantity), Some(order_status),
        Some(asin), unit_price_tax, shipping_charge, total_discounts, total_owed,
        shipment_item_subtotal, shipment_item_subtotal_tax, product_condition,
        payment_instrument_type, shipment_status, ship_date, shipping_option,
        carrier_tracking, shipping_address, billing_address, gift_message,
        gift_sender_name, gift_recipient, item_serial_number,
        idx("purchase order number"), // tracked but goes raw-only
    ] {
        if let Some(i) = i { mapped.insert(i); }
    }

    Ok(Cols {
        website, order_id, order_date, currency, unit_price, unit_price_tax,
        shipping_charge, total_discounts, total_owed, shipment_item_subtotal,
        shipment_item_subtotal_tax, asin, product_condition, quantity,
        payment_instrument_type, order_status, shipment_status, ship_date,
        shipping_option, carrier_tracking, product_name,
        shipping_address, billing_address, gift_message, gift_sender_name,
        gift_recipient, item_serial_number, mapped,
    })
}

// ---------------------------------------------------------------------------
// The import function.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = order_history_csv(path)?;
    // Strip BOM if present (the first sample starts with U+FEFF).
    let body = body.trim_start_matches('\u{feff}');

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());

    let headers: Vec<String> = rdr
        .headers()
        .context("reading CSV header row")?
        .iter()
        .map(|s| s.to_string())
        .collect();
    let cols = detect_cols(&headers)?;

    // Load existing guids for dedupe (idempotent re-import).
    let stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen_guids: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for item in stream.read::<LineItem>(&key)? {
            if !item.guid.is_empty() {
                seen_guids.insert(item.guid);
            }
        }
    }

    // Per-order line index: `<OrderID>` → next index. This yields stable guids
    // across re-imports because the CSV preserves insertion order per order.
    let mut order_line_idx: HashMap<String, usize> = HashMap::new();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut contract_rows: Vec<LineItem> = Vec::new();
    let mut raw_rows: Vec<RawLine> = Vec::new();

    for record in rdr.records() {
        let record = record.context("reading CSV row")?;
        rows += 1;
        let get = |i: usize| record.get(i).unwrap_or("").trim();
        let getopt = |opt: Option<usize>| opt.map(|i| get(i)).unwrap_or("").trim();

        // Required: Order ID and Order Date.
        let order_id = get(cols.order_id);
        let order_date_raw = get(cols.order_date);
        if order_id.is_empty() || order_date_raw.is_empty() {
            skipped += 1;
            continue;
        }
        let Some(ts) = parse_order_date(order_date_raw) else {
            skipped += 1;
            continue;
        };

        // Stable guid: OrderID + colon + per-order line index.
        let line_idx = order_line_idx.entry(order_id.to_string()).or_insert(0);
        let guid = format!("{order_id}:{line_idx}");
        *line_idx += 1;

        if seen_guids.contains(&guid) {
            duplicates += 1;
            // Still need to advance any subsequent guids in the same order.
            continue;
        }

        // Raw row: all columns as a JSON object, keyed by normalized header name.
        let mut raw_obj = serde_json::Map::new();
        for (i, header) in headers.iter().enumerate() {
            let v = get(i);
            if !v.is_empty() {
                raw_obj.insert(
                    norm_header(header),
                    Value::String(v.to_string()),
                );
            }
        }
        // Tag with vault guid so the raw layer can be cross-referenced.
        raw_obj.insert("_guid".into(), Value::String(guid.clone()));

        // Contract row.
        let website = na_to_empty(get(cols.website));
        // Merchant: "Amazon" for standard rows; use the website value when it's
        // something other than "Amazon.com" (e.g. a third-party storefront).
        let merchant = if website.eq_ignore_ascii_case("amazon.com") || website.is_empty() {
            "Amazon".to_string()
        } else {
            website.clone()
        };

        let product_name = na_to_empty(get(cols.product_name));
        let currency = na_to_empty(get(cols.currency));
        let asin = na_to_empty(get(cols.asin));
        let order_status = na_to_empty(get(cols.order_status)).to_ascii_lowercase();

        let qty = get(cols.quantity).parse::<f64>().ok();
        let unit_price = parse_amount(get(cols.unit_price));
        // amount = Shipment Item Subtotal (per-line pre-tax total = qty × unit_price).
        // This satisfies the finance-purchases schema: amount = unit_price × qty,
        // before order-level shipping/tax. Total Owed (order-level charged total,
        // which includes tax + shipping − discounts) goes into extra.total_owed.
        let shipment_item_subtotal = cols.shipment_item_subtotal.and_then(|i| parse_amount(get(i)));
        let total_owed = cols.total_owed.and_then(|i| parse_amount(get(i)));

        // Shipment map.
        let shipment = make_shipment(
            getopt(cols.carrier_tracking),
            getopt(cols.ship_date),
            getopt(cols.shipment_status),
            getopt(cols.shipping_option),
        );

        // Extra: source-specific fields that don't fit the contract columns.
        let mut extra = Map::new();
        macro_rules! put_str {
            ($k:expr, $v:expr) => {{
                let v = na_to_empty($v);
                if !v.is_empty() { extra.insert($k.into(), Value::String(v)); }
            }};
        }
        macro_rules! put_num {
            ($k:expr, $v:expr) => {{
                if let Some(n) = $v { extra.insert($k.into(), Value::from(n)); }
            }};
        }

        put_str!("asin", &asin);
        if !website.eq_ignore_ascii_case("amazon.com") && !website.is_empty() {
            put_str!("website", &website);
        }
        put_num!("unit_price_tax", cols.unit_price_tax.and_then(|i| parse_amount(get(i))));
        put_num!("shipping_charge", cols.shipping_charge.and_then(|i| parse_amount(get(i))));
        put_num!("total_discounts", cols.total_discounts.and_then(|i| parse_amount(get(i))));
        // total_owed = the complete charged total (subtotal + tax + shipping − discounts).
        // Preserved here for fidelity; the schema-correct line amount is in `amount`.
        put_num!("total_owed", total_owed);
        put_num!("shipment_item_subtotal_tax", cols.shipment_item_subtotal_tax.and_then(|i| parse_amount(get(i))));
        if let Some(i) = cols.product_condition { put_str!("product_condition", get(i)); }
        if let Some(i) = cols.payment_instrument_type { put_str!("payment_instrument_type", get(i)); }
        // Item serial number (present in newer exports).
        if let Some(i) = cols.item_serial_number { put_str!("item_serial_number", get(i)); }
        // Unmapped, non-empty columns go into extra verbatim.
        for (i, header) in headers.iter().enumerate() {
            if !cols.mapped.contains(&i) {
                let v = get(i);
                if !v.is_empty() {
                    extra.insert(norm_header(header), Value::String(v.to_string()));
                }
            }
        }

        let mut row = LineItem::new("amazon", &guid, &ts, &merchant);
        row.order_id = order_id.to_string();
        row.item = product_name;
        row.qty = qty;
        row.unit_price = unit_price;
        row.amount = shipment_item_subtotal; // per-line pre-tax subtotal (= qty × unit_price)
        row.currency = currency;
        row.status = order_status;
        row.shipment = shipment;
        row.extra = extra;

        seen_guids.insert(guid.clone());
        contract_rows.push(row);
        raw_rows.push(RawLine { ts, value: Value::Object(raw_obj) });
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    stream.append(&contract_rows, |r| &r.ts)?;
    raw_stream.append(&raw_rows, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} order lines imported, {duplicates} duplicates skipped"),
        counts: BTreeMap::from([
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::newest_stem;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-amazon-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixture: synthetic CSV matching the CONFIRMED real-export shape.
    //
    // Header columns verified against two real Privacy Central exports (2023, 2025).
    // Data values are synthetic (no PII on disk).

    const HEADER: &str = "\"Website\",\"Order ID\",\"Order Date\",\"Purchase Order Number\",\
        \"Currency\",\"Unit Price\",\"Unit Price Tax\",\"Shipping Charge\",\"Total Discounts\",\
        \"Total Owed\",\"Shipment Item Subtotal\",\"Shipment Item Subtotal Tax\",\
        \"ASIN\",\"Product Condition\",\"Quantity\",\"Payment Instrument Type\",\
        \"Order Status\",\"Shipment Status\",\"Ship Date\",\"Shipping Option\",\
        \"Shipping Address\",\"Billing Address\",\"Carrier Name & Tracking Number\",\
        \"Product Name\",\"Gift Message\",\"Gift Sender Name\",\"Gift Recipient Contact Details\"";

    // Two items in the same order, plus a standalone order.
    //
    // Amounts are internally consistent: Shipment Item Subtotal = Unit Price × Quantity
    // (the finance-purchases contract invariant for `amount`). Total Owed is the order-level
    // charged total (subtotal + tax + shipping − discounts) and goes into extra.total_owed.
    //
    // Row 1: unit_price=35.99, qty=2 → subtotal=71.98, tax=6.12, total_owed=78.10
    // Row 2: unit_price=12.99, qty=1 → subtotal=12.99, tax=1.10, total_owed=14.09
    // Row 3: unit_price=8.49, qty=3 → subtotal=25.47, discount=-1.00, total_owed=24.47
    fn csv_body() -> String {
        format!(
            "{HEADER}\n\
            \"Amazon.com\",\"111-1111111-1111111\",\"2024-06-15T10:00:00Z\",\"Not Applicable\",\
            \"USD\",\"35.99\",\"3.06\",\"0\",\"0\",\"78.10\",\"71.98\",\"6.12\",\
            \"B08D6T6N9C\",\"New\",\"2\",\"Visa - 8860\",\
            \"Closed\",\"Shipped\",\"2024-06-16T08:00:00Z\",\"second-day\",\
            \"123 Main St Anytown CA 90210\",\"123 Main St Anytown CA 90210\",\
            \"AMZL_US(TBA303012345678)\",\
            \"Anker USB-C Charger 65W\",\"Not Available\",\"Not Available\",\"Not Available\"\n\
            \"Amazon.com\",\"111-1111111-1111111\",\"2024-06-15T10:00:00Z\",\"Not Applicable\",\
            \"USD\",\"12.99\",\"1.10\",\"0\",\"0\",\"14.09\",\"12.99\",\"1.10\",\
            \"B01NAOI0D9\",\"New\",\"1\",\"Visa - 8860\",\
            \"Closed\",\"Shipped\",\"2024-06-16T08:00:00Z\",\"second-day\",\
            \"123 Main St Anytown CA 90210\",\"123 Main St Anytown CA 90210\",\
            \"AMZL_US(TBA303012345678)\",\
            \"USB Extension Cable 10ft\",\"Not Available\",\"Not Available\",\"Not Available\"\n\
            \"Amazon.com\",\"222-2222222-2222222\",\"2024-07-04T14:30:00Z\",\"Not Applicable\",\
            \"USD\",\"8.49\",\"0\",\"0\",\"'-1.00'\",\"24.47\",\"25.47\",\"0\",\
            \"B000T6AHW6\",\"New\",\"3\",\"Gift Certificate/Card and Visa - 8860\",\
            \"Cancelled\",\"Not Available\",\"Not Available\",\"Not Available\",\
            \"Not Available\",\"Not Available\",\"Not Available\",\
            \"Lavender Syrup 750ml\",\"Not Available\",\"Not Available\",\"Not Available\"\n"
        )
    }

    /// Write the fixture CSV to a temp file and run the import.
    fn import_csv(v: &Vault, body: &str) -> ImportOutcome {
        let path = v.root().join("Retail.OrderHistory.1.csv");
        fs::write(&path, body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // --- unit: parse_order_date ---

    #[test]
    fn parse_order_date_utc_iso8601_to_local_midnight() {
        // "2023-11-12T04:20:39Z" → RFC3339 local midnight. The exact date depends
        // on the machine's timezone (UTC-8 Pacific → Nov 11, UTC+0 → Nov 12), so
        // we only assert the result is valid RFC3339 and is at midnight (T00:00:00).
        let ts = parse_order_date("2023-11-12T04:20:39Z").unwrap();
        assert!(DateTime::parse_from_rfc3339(&ts).is_ok(), "valid RFC3339: {ts}");
        // Output is midnight in the local zone.
        assert!(ts.contains("T00:00:00"), "midnight in local zone: {ts}");
        // The date portion (first 10 chars) is a valid YYYY-MM-DD.
        let date_part = &ts[..10];
        assert!(
            chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d").is_ok(),
            "valid date portion: {ts}"
        );
    }

    #[test]
    fn parse_order_date_fractional_seconds_ok() {
        // Confirmed format from newer exports.
        let ts = parse_order_date("2025-11-02T15:41:10.586Z").unwrap();
        assert!(ts.starts_with("2025-11-02"), "date preserved: {ts}");
    }

    #[test]
    fn parse_order_date_rejects_empty() {
        assert!(parse_order_date("").is_none());
        assert!(parse_order_date("not available").is_none());
    }

    // --- unit: parse_amount ---

    #[test]
    fn parse_amount_handles_quoted_negative_discounts() {
        // Amazon wraps negative discounts: "'-1.34'"
        assert_eq!(parse_amount("'-1.34'"), Some(-1.34));
        assert_eq!(parse_amount("0"), Some(0.0));
        assert_eq!(parse_amount("35.99"), Some(35.99));
        assert_eq!(parse_amount("Not Available"), None);
        assert_eq!(parse_amount(""), None);
    }

    // --- unit: parse_tracking ---

    #[test]
    fn parse_tracking_extracts_carrier_and_number() {
        let (tr, ca) = parse_tracking("AMZL_US(TBA303012345678)");
        assert_eq!(tr.as_deref(), Some("TBA303012345678"));
        assert_eq!(ca.as_deref(), Some("AMZL_US"));

        let (tr2, ca2) = parse_tracking("UPS(1Z12345E0291980793)");
        assert_eq!(tr2.as_deref(), Some("1Z12345E0291980793"));
        assert_eq!(ca2.as_deref(), Some("UPS"));

        let (tr3, ca3) = parse_tracking("Not Available");
        assert!(tr3.is_none());
        assert!(ca3.is_none());
    }

    // --- unit: column detection ---

    #[test]
    fn detect_cols_recognizes_confirmed_header() {
        let headers: Vec<String> = HEADER
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .collect();
        let cols = detect_cols(&headers).unwrap();
        // Spot-check a few required columns.
        assert_eq!(&headers[cols.order_id], "Order ID");
        assert_eq!(&headers[cols.asin], "ASIN");
        assert!(cols.carrier_tracking.is_some());
        assert!(cols.unit_price_tax.is_some());
    }

    #[test]
    fn detect_cols_rejects_non_amazon_csv() {
        let bad: Vec<String> = vec!["Date".into(), "Amount".into(), "Description".into()];
        assert!(detect_cols(&bad).is_err());
    }

    // --- integration: import + dedupe ---

    #[test]
    fn imports_contract_and_raw_with_correct_guids() {
        let v = temp_vault("basic");
        let out = import_csv(&v, &csv_body());
        assert_eq!(out.counts.get("imported"), Some(&3), "3 line items");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("skipped"), Some(&0));

        // Contract rows: first two in 2024-06, third in 2024-07.
        let stream = v.stream(DIR, Partition::Month);
        let june: Vec<LineItem> = stream.read("2024-06").unwrap();
        assert_eq!(june.len(), 2, "two lines in same order in June");
        let july: Vec<LineItem> = stream.read("2024-07").unwrap();
        assert_eq!(july.len(), 1, "one line in July order");

        // Guid format.
        assert_eq!(june[0].guid, "111-1111111-1111111:0", "first line of order = :0");
        assert_eq!(june[1].guid, "111-1111111-1111111:1", "second line = :1");
        assert_eq!(july[0].guid, "222-2222222-2222222:0");

        // Contract field mapping (from first row).
        let first = &june[0];
        assert_eq!(first.source, "amazon");
        assert_eq!(first.merchant, "Amazon");
        assert_eq!(first.order_id, "111-1111111-1111111");
        assert_eq!(first.item, "Anker USB-C Charger 65W");
        assert_eq!(first.qty, Some(2.0));
        assert_eq!(first.unit_price, Some(35.99));
        // amount = Shipment Item Subtotal = unit_price × qty (35.99 × 2 = 71.98).
        assert_eq!(first.amount, Some(71.98), "amount is the per-line pre-tax subtotal");
        assert_eq!(first.currency, "USD");
        assert_eq!(first.status, "closed");
        // Shipment.
        assert_eq!(
            first.shipment.get("tracking").and_then(Value::as_str),
            Some("TBA303012345678")
        );
        assert_eq!(
            first.shipment.get("carrier").and_then(Value::as_str),
            Some("AMZL_US")
        );
        // Extra.
        assert_eq!(first.extra.get("asin").and_then(Value::as_str), Some("B08D6T6N9C"));
        assert_eq!(first.extra.get("unit_price_tax"), Some(&serde_json::json!(3.06)));
        // total_owed (the complete order-level charge) is in extra, not amount.
        assert_eq!(first.extra.get("total_owed"), Some(&serde_json::json!(78.10)));
        // shipment_item_subtotal is now the contract `amount`, not an extra field.
        assert!(first.extra.get("shipment_item_subtotal").is_none(),
            "subtotal is now `amount`, not extra");

        // Cancelled row: negative discount parsed, status "cancelled".
        let cancelled = &july[0];
        assert_eq!(cancelled.status, "cancelled");
        assert_eq!(cancelled.extra.get("total_discounts"), Some(&serde_json::json!(-1.0)));

        // Raw layer: same count, includes full fields.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let raw_june: Vec<Value> = raw_stream.read("2024-06").unwrap();
        assert_eq!(raw_june.len(), 2, "raw mirrors contract count");
        // Raw carries billing address (which stays out of the contract).
        let raw_first = &raw_june[0];
        assert!(raw_first.get("billing address").is_some(), "billing address in raw");
        // Raw is tagged with _guid.
        assert_eq!(
            raw_first.get("_guid").and_then(Value::as_str),
            Some("111-1111111-1111111:0")
        );
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let first = import_csv(&v, &csv_body());
        assert_eq!(first.counts.get("imported"), Some(&3));

        // Re-import: all rows are duplicates.
        let second = import_csv(&v, &csv_body());
        assert_eq!(second.counts.get("imported"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&3));

        // Contract files are byte-identical after the re-import.
        let stream = v.stream(DIR, Partition::Month);
        for key in stream.partitions().unwrap() {
            let path = v.root().join(format!("{DIR}/{key}.jsonl"));
            let before = fs::read(&path).unwrap();
            import_csv(&v, &csv_body());
            let after = fs::read(&path).unwrap();
            assert_eq!(before, after, "file byte-identical after re-import");
        }
    }

    #[test]
    fn import_from_zip_accepts_canonical_path() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("amazon-orders.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Retail.OrderHistory.1/Retail.OrderHistory.1.csv", opts).unwrap();
        w.write_all(csv_body().as_bytes()).unwrap();
        // A decoy that must not be read.
        w.start_file("other/Retail.OrderHistory.1.csv", opts).unwrap();
        w.write_all(b"junk\n").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3), "reads from canonical ZIP path");
    }

    #[test]
    fn import_from_zip_falls_back_to_any_matching_entry() {
        use std::io::Write;
        let v = temp_vault("zip-fallback");
        let zip_path = v.root().join("amazon-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // Non-canonical path but matching name.
        w.start_file("renamed/Retail.OrderHistory.1.csv", opts).unwrap();
        w.write_all(csv_body().as_bytes()).unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3), "falls back to any matching entry");
    }

    #[test]
    fn import_from_zip_accepts_dot2_variant() {
        // Amazon also ships a Retail.OrderHistory.2/Retail.OrderHistory.2.csv variant.
        use std::io::Write;
        let v = temp_vault("zip-dot2");
        let zip_path = v.root().join("amazon-orders-v2.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Retail.OrderHistory.2/Retail.OrderHistory.2.csv", opts).unwrap();
        w.write_all(csv_body().as_bytes()).unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3), "reads from .2 ZIP variant");
    }

    #[test]
    fn newer_export_with_extra_columns_still_parses() {
        // The 2025 export adds "Item Serial Number" at the end.
        let header_with_serial = format!("{HEADER},\"Item Serial Number\"");
        let body = format!(
            "{header_with_serial}\n\
            \"Amazon.com\",\"333-3333333-3333333\",\"2025-03-10T12:00:00Z\",\"Not Applicable\",\
            \"USD\",\"49.99\",\"4.25\",\"0\",\"0\",\"54.24\",\"49.99\",\"4.25\",\
            \"B0FRFPV5FK\",\"New\",\"1\",\"Visa - 8860\",\
            \"Closed\",\"Shipped\",\"2025-03-11T10:00:00Z\",\"next-day\",\
            \"123 Main St\",\"123 Main St\",\
            \"UPS(1Z12345E0291980793)\",\
            \"Smart Plug 2-Pack\",\"Not Available\",\"Not Available\",\"Not Available\",\
            \"SN12345678\"\n"
        );
        let v = temp_vault("extra-col");
        let out = import_csv(&v, &body);
        assert_eq!(out.counts.get("imported"), Some(&1));
        let stream = v.stream(DIR, Partition::Month);
        let rows: Vec<LineItem> = stream.read("2025-03").unwrap();
        assert_eq!(rows.len(), 1);
        // Serial number in extra.
        assert_eq!(
            rows[0].extra.get("item_serial_number").and_then(Value::as_str),
            Some("SN12345678")
        );
    }

    #[test]
    fn bom_prefix_is_stripped() {
        let v = temp_vault("bom");
        // Prepend BOM (as seen in the 2023 real export).
        let body = format!("\u{feff}{}", csv_body());
        let out = import_csv(&v, &body);
        assert_eq!(out.counts.get("imported"), Some(&3), "BOM stripped: {out:?}");
    }

    #[test]
    fn def_points_to_amazon_import_spec() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.id, "amazon");
        assert_eq!(DEF.meta.vault_path, "finance/purchases/amazon/");
        assert!(DEF.connection.is_none(), "no login needed");
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"zip"));
        assert!(spec.accepts.contains(&"csv"));
    }

    #[test]
    fn last_data_reflects_newest_partition() {
        let v = temp_vault("lastdata");
        import_csv(&v, &csv_body());
        let ld = def_last_data(&v);
        // We have data in 2024-06 and 2024-07; newest should be 2024-07.
        assert_eq!(ld.as_deref(), Some("2024-07"));

        // Convenience: newest_stem is used consistently (letterboxd pattern).
        let ld2 = newest_stem(&v.root().join(DIR));
        assert_eq!(ld, ld2);
    }
}
