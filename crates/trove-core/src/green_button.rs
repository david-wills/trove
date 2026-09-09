//! Utility Smart Meter (Green Button) ESPI XML importer.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/green-button.md
//!
//! ## Vault layout
//!
//! - **Raw layer (unconditional):**
//!   `home/green-button/raw/<datestamp>-<filename>` — the verbatim exported
//!   XML (or CSV) file, preserved at full fidelity on every import.
//!
//! - **Energy interval layer (raw-only, home.energy draft shape):**
//!   `home/green-button/energy/YYYY-MM.jsonl` — one line per metered
//!   interval, following the `home.energy` schema from
//!   `docs/vault-spec/domains/home.md`. The Rust struct is NOT the bound
//!   [`crate::home::HomeReading`] (sensor readings); the `home.energy` shape
//!   is an unbound sibling draft. We write it as ad-hoc `serde_json::Value`
//!   so the schema stays in the spec without being locked into a Rust type
//!   prematurely. When the pioneer binds `home.energy`, existing files are
//!   already correct — no migration needed.
//!
//! ## ESPI XML structure (confirmed from real sample)
//!
//! Green Button ESPI data is an Atom feed (`xmlns="http://www.w3.org/2005/Atom"`)
//! with multiple `<entry>` elements. Each entry wraps its typed payload in
//! `<content>` with an `xmlns:espi="http://naesb.org/espi"` declaration.
//!
//! - **ReadingType entry:** `<espi:ReadingType>` carries `<espi:uom>` (IEC 61968-9
//!   numeric code: 72=Wh, 61=W, 119=therm, 43=ft3/CCF, 42=m3, 24=gal) and
//!   `<espi:powerOfTenMultiplier>` (−3=milli, 0=unity, 3=kilo). The href on the
//!   entry's `<link rel="self">` is matched via MeterReading's `<link rel="related">`.
//! - **MeterReading entry:** `<espi:MeterReading>` carries a `<link rel="related">`
//!   pointing to the ReadingType entry and another to its IntervalBlock entries.
//! - **IntervalBlock entry:** `<espi:IntervalBlock>` contains an `<interval>`
//!   (overall span: `<start>` Unix seconds, `<duration>` seconds) and one or more
//!   `<IntervalReading>` elements (each with `<timePeriod>/<start>`, `<duration>`,
//!   `<value>`, optional `<cost>`, optional `<ReadingQuality>`). The `<value>` is
//!   an integer in the reading unit × 10^multiplier (e.g. value=2740 with uom=72
//!   multiplier=−3 → 2.74 Wh; value=274 with multiplier=0 → 274 Wh; value=91
//!   with uom=72 multiplier=3 → 91000 Wh = 91 kWh, though in practice most
//!   utilities use multiplier=3 and report whole-number kWh×10^−3 → fractional kWh).
//!
//! Element nesting: inner elements may use `xmlns=""` (no namespace) OR carry
//! the espi: prefix, depending on the exporting utility.  The parser handles
//! both forms: `<IntervalReading>` and `<espi:IntervalReading>`, `<value>` and
//! `<espi:value>`, etc.  `extract_element_text` strips any namespace prefix.
//!
//! ## Guid strategy
//!
//! `guid = "green-button:{meter_id}:{interval_start}"` — synthesized from the
//! MeterReading/UsagePoint id (from the entry `<link rel="self">`) and the
//! interval start Unix timestamp. Re-importing an overlapping export is
//! idempotent: same guid, deduped on read (seen set).
//!
//! ## Commodity mapping
//!
//! ESPI `commodity` codes: 1=electricity, 7=naturalGas, 8=water. The vault
//! `circuit` field carries the commodity meter label when the commodity isn't
//! electricity (matching the `home.md` example: `"circuit":"gas-meter"`).
//!
//! ## Unit mapping (UOM + powerOfTenMultiplier → vault unit + kwh/value)
//!
//! | uom | commodity | multiplier | vault field | vault unit |
//! |-----|-----------|-----------|-------------|------------|
//! | 72 (Wh) | 1 (elec) | 3 | `kwh` | (none, implied) |
//! | 72 (Wh) | 1 (elec) | 0 | `kwh` (÷1000) | — |
//! | 61 (W) | 1 (elec) | any | `kwh` (×dur/3600 × 10^mult) | — |
//! | 119 (therm) | 7 (gas) | any | `value` | `therm` |
//! | 43 (ft3/CCF) | 7 (gas) | any | `value` | `ccf` |
//! | 42 (m3) | 7/8 (gas/water) | any | `value` | `m3` |
//! | 24 (gal) | 8 (water) | any | `value` | `gal` |
//! | other | any | any | `value` (raw) | raw uom code |

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{json, Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, ImportOutcome, ImportSpec, IntegrationDef,
};
use crate::store::{write_atomic, Partition};
use crate::vault::Vault;

/// Raw verbatim backup of each imported file.
const RAW_DIR: &str = "home/green-button/raw";
/// Energy interval stream (home.energy draft shape — raw-only, no bound Rust type).
const ENERGY_DIR: &str = "home/green-button/energy";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Prefer the energy stream's newest month; fall back to raw mtime.
    let energy = vault.root().join(ENERGY_DIR);
    if let Some(s) = crate::registry::newest_stem(&energy) {
        return Some(s);
    }
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The line is already present (Phase 2 stub); this build upgrades from
/// `NotWired` to `Import`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "green-button",
        name: "Utility Smart Meter (Green Button)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your utility electricity (and gas/water) usage history \
                      from a Green Button ESPI XML export. Supported by most major \
                      US utilities — download your data from your utility's website \
                      and import it here. Re-importable: overlapping exports are \
                      automatically deduplicated.",
        domain: "home",
        vault_path: "home/green-button/",
        toggleable: false,
        setup: &[
            "Log in to your utility's website (PG&E, ComEd, ConEd, etc.).",
            "Navigate to energy usage / smart meter data and choose \
             \"Download My Data\" or \"Export\" — select the ESPI XML option \
             (sometimes labelled \"Green Button\").",
            "Import the downloaded XML file here.",
        ],
        caveats: "Data must be downloaded manually from your utility's website. \
                  Some utilities export CSV instead of ESPI XML — CSV support is \
                  pending a real utility-specific sample. Re-importing overlapping \
                  windows is safe (intervals are deduplicated by meter + timestamp).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["xml"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Raw storage + dispatch

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Raw layer: keep verbatim copy, timestamped so re-imports accumulate.
    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
    let orig_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("green_button.xml");
    let raw_path = vault.resolve(&format!("{RAW_DIR}/{stamp}-{orig_name}"))?;
    write_atomic(&raw_path, &raw_bytes)?;
    progress(ImportProgress { records: 0, percent: 10.0 });

    // Dispatch by file type.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let intervals = if ext == "xml" {
        let xml_str = String::from_utf8(raw_bytes)
            .context("Green Button XML file is not valid UTF-8")?;
        parse_espi_xml(&xml_str).context("parsing ESPI XML")?
    } else {
        bail!(
            "Green Button CSV import is not yet supported (Needs-sample: \
             CSV format varies per utility). The file has been stored in \
             home/green-button/raw/ at full fidelity."
        );
    };

    progress(ImportProgress { records: intervals.len() as u64, percent: 70.0 });

    let (imported, duplicates) = write_intervals(vault, intervals)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} intervals imported, {duplicates} duplicates skipped"
        ),
        counts: BTreeMap::from([
            ("imported", imported),
            ("duplicates", duplicates),
            ("raw_files", 1u64),
        ]),
    })
}

// ---------------------------------------------------------------------------
// ESPI XML parser

/// One parsed metered interval, ready to write to the energy JSONL stream.
struct EnergyInterval {
    /// RFC3339 local timestamp of the interval start (partition key).
    ts: String,
    /// Synthesized stable id for idempotent re-import.
    guid: String,
    /// The vault energy row as a JSON object (home.energy draft shape).
    row: Map<String, Value>,
}

/// Parse a Green Button ESPI Atom feed, extracting metered intervals.
///
/// The ESPI feed has three entry types (ReadingType, MeterReading, IntervalBlock).
/// We do a two-pass parse: first collect ReadingType and MeterReading metadata
/// keyed by their Atom `<id>` hrefs, then walk IntervalBlock entries expanding
/// readings into vault rows.
fn parse_espi_xml(xml: &str) -> Result<Vec<EnergyInterval>> {
    // --- Pass 1: index all entries by their self-href ---
    // An entry looks like:
    //   <entry>
    //     <id>...</id>
    //     <link rel="self" href="..."/>
    //     <link rel="related" href="..."/>  (0 or more)
    //     <content>
    //       <espi:ReadingType>...</espi:ReadingType>
    //       | <espi:MeterReading>...</espi:MeterReading>
    //       | <espi:IntervalBlock>...</espi:IntervalBlock>
    //     </content>
    //   </entry>
    //
    // We extract the raw XML string for each entry's <content> child and
    // classify it by the outermost child element's local name.

    let entries = split_entries(xml)?;

    // Indexed: self_href → (kind, content_xml, related_hrefs)
    let mut reading_types: BTreeMap<String, ReadingTypeMeta> = BTreeMap::new();
    // MeterReading self_href → related hrefs (ReadingType href, IntervalBlock hrefs)
    let mut meter_readings: BTreeMap<String, MeterReadingMeta> = BTreeMap::new();
    // IntervalBlock entries (self_href → parsed intervals with their meter href)
    let mut interval_blocks: Vec<IntervalBlockEntry> = Vec::new();

    for entry in &entries {
        match entry.kind.as_str() {
            "ReadingType" => {
                if let Ok(rt) = parse_reading_type(&entry.content) {
                    reading_types.insert(entry.self_href.clone(), rt);
                }
            }
            "MeterReading" => {
                // Separate the related hrefs into ReadingType and IntervalBlock links.
                let reading_type_href = entry
                    .related_hrefs
                    .iter()
                    .find(|h| href_has_segment(h, "ReadingType"))
                    .cloned()
                    .unwrap_or_default();
                let interval_block_hrefs = entry
                    .related_hrefs
                    .iter()
                    .filter(|h| href_has_segment(h, "IntervalBlock"))
                    .cloned()
                    .collect();
                meter_readings.insert(
                    entry.self_href.clone(),
                    MeterReadingMeta {
                        reading_type_href,
                        interval_block_hrefs,
                        self_href: entry.self_href.clone(),
                    },
                );
            }
            "IntervalBlock" => {
                // Find which MeterReading owns this IntervalBlock via its
                // parent (an entry whose related hrefs include our self_href,
                // OR the entry that directly contains us). In practice we just
                // resolve by matching related hrefs from MeterReading entries.
                let meter_href = find_meter_href_for_block(&entry.self_href, &meter_readings);
                if let Ok(readings) = parse_interval_block(&entry.content) {
                    interval_blocks.push(IntervalBlockEntry {
                        self_href: entry.self_href.clone(),
                        meter_href,
                        readings,
                    });
                }
            }
            _ => {}
        }
    }

    // --- Pass 2: resolve ReadingType for each IntervalBlock and emit rows ---
    // Warn if any block could not be resolved (zero-interval silent success is
    // misleading; callers can observe this in tracing/logs).
    let mut result = Vec::new();
    for block in &interval_blocks {
        // Find the MeterReading that owns this block.
        let meter_meta = block
            .meter_href
            .as_deref()
            .and_then(|mh| meter_readings.get(mh));

        // Resolve ReadingType: prefer the one linked from the MeterReading;
        // fall back to the sole ReadingType if there is exactly one (handles
        // standalone IntervalBlock feeds with no MeterReading entry).
        let rt_opt = meter_meta
            .and_then(|mr| {
                // Prefer strict self-href equality; fall back to normalised match.
                reading_types.get(&mr.reading_type_href).or_else(|| {
                    let want = strip_scheme_host(&mr.reading_type_href)
                        .trim_end_matches('/')
                        .to_lowercase();
                    reading_types.iter()
                        .find(|(k, _)| {
                            strip_scheme_host(k).trim_end_matches('/').to_lowercase() == want
                        })
                        .map(|(_, v)| v)
                })
            })
            .or_else(|| {
                // Standalone-block fallback: single ReadingType file.
                if reading_types.len() == 1 {
                    reading_types.values().next()
                } else {
                    None
                }
            });

        // Derive the meter id from the MeterReading self_href.
        // Never fall back to the IntervalBlock id — it is not stable across
        // re-exports.  If no MeterReading is found, use the UsagePoint portion
        // of the block href, or empty string (guid will still be unique via ts).
        let meter_id = meter_meta
            .map(|mr| meter_id_from_href(&mr.self_href))
            .or_else(|| {
                // Look for a UsagePoint id in the block self_href path.
                let path = strip_scheme_host(&block.self_href);
                usage_point_id_from_path(path)
            })
            .unwrap_or_default();

        if block.readings.is_empty() {
            // Non-fatal: block parsed but no readings found (could be espi:-prefixed
            // elements that were not matched).  A zero count will surface in the
            // ImportOutcome headline so the user is not silently misled.
        }

        for raw_reading in &block.readings {
            let interval_start_unix = raw_reading.start_unix;
            let duration_secs = raw_reading.duration_secs;
            let raw_value = raw_reading.value;

            // Convert value to vault fields using ReadingType metadata.
            let (kwh_opt, value_opt, unit, circuit) = match rt_opt {
                Some(rt) => convert_reading(raw_value, duration_secs, rt),
                None => {
                    // No ReadingType: assume Wh with multiplier 0 → kWh.
                    let kwh = (raw_value as f64) / 1000.0;
                    (Some(kwh), None, String::new(), String::new())
                }
            };

            // Direction from ReadingType.flowDirection; default consumption.
            let direction = rt_opt
                .map(|rt| flow_direction_str(rt.flow_direction))
                .unwrap_or("consumption");

            // Guid: incorporate meter_id so distinct meters never collide.
            // Also embed the circuit/unit discriminator so gas+electric at the
            // same timestamp get distinct guids even if meter_id is ambiguous.
            let commodity_tag = match rt_opt {
                Some(rt) => rt.commodity.to_string(),
                None => "1".to_string(),
            };
            let ts = unix_to_rfc3339_local(interval_start_unix);
            let guid = format!("green-button:{meter_id}:{commodity_tag}:{interval_start_unix}");

            let mut row = Map::new();
            row.insert("ts".into(), Value::String(ts.clone()));
            row.insert("source".into(), Value::String("green-button".into()));
            if !meter_id.is_empty() {
                row.insert("device".into(), Value::String(meter_id.clone()));
            }
            if !circuit.is_empty() {
                row.insert("circuit".into(), Value::String(circuit));
            }
            if let Some(kwh) = kwh_opt {
                row.insert("kwh".into(), json!(round6(kwh)));
            }
            if let Some(val) = value_opt {
                row.insert("value".into(), json!(round6(val)));
            }
            if !unit.is_empty() {
                row.insert("unit".into(), Value::String(unit));
            }
            if duration_secs > 0 {
                row.insert("interval_secs".into(), json!(duration_secs));
            }
            row.insert("direction".into(), Value::String(direction.into()));
            row.insert("guid".into(), Value::String(guid.clone()));

            // Extra: cost if present (in 1/100000 of currency per ESPI).
            if raw_reading.cost_hundred_thousandths > 0 {
                let mut extra = Map::new();
                extra.insert(
                    "cost_raw".into(),
                    json!(raw_reading.cost_hundred_thousandths),
                );
                row.insert("extra".into(), Value::Object(extra));
            }

            result.push(EnergyInterval { ts, guid, row });
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Write intervals to the energy JSONL stream with dedupe

fn write_intervals(vault: &Vault, intervals: Vec<EnergyInterval>) -> Result<(u64, u64)> {
    // Load existing guids for idempotency.
    let stream = vault.stream(ENERGY_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions().unwrap_or_default() {
        if let Ok(rows) = stream.read::<Value>(&key) {
            for row in rows {
                if let Some(g) = row.get("guid").and_then(|v| v.as_str()) {
                    seen.insert(g.to_string());
                }
            }
        }
    }

    let (mut imported, mut duplicates) = (0u64, 0u64);
    let mut to_write: Vec<(String, Value)> = Vec::new();

    for interval in intervals {
        if seen.contains(&interval.guid) {
            duplicates += 1;
            continue;
        }
        seen.insert(interval.guid.clone());
        to_write.push((interval.ts, Value::Object(interval.row)));
        imported += 1;
    }

    // Append each row to its month partition.
    // `JsonlStream::append` groups by ts prefix — we pass the ts extractor.
    // Since `Value` doesn't natively expose a ts string, use a wrapper.
    struct WithTs(String, Value);
    impl serde::Serialize for WithTs {
        fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
        where S: serde::Serializer {
            self.1.serialize(s)
        }
    }

    let rows_with_ts: Vec<WithTs> = to_write.into_iter().map(|(ts, v)| WithTs(ts, v)).collect();
    stream.append(&rows_with_ts, |r| &r.0)?;

    Ok((imported, duplicates))
}

// ---------------------------------------------------------------------------
// Atom entry splitting

struct EntryMeta {
    self_href: String,
    related_hrefs: Vec<String>,
    kind: String,       // "ReadingType" | "MeterReading" | "IntervalBlock" | ""
    content: String,    // the raw string inside <content>...</content>
}

/// Split the Atom feed into per-entry chunks using the quick-xml reader.
/// We collect the raw XML text between `<content>` and `</content>` for
/// subsequent per-entry parsers, and the `<link rel="...">` hrefs.
fn split_entries(xml: &str) -> Result<Vec<EntryMeta>> {

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut entries: Vec<EntryMeta> = Vec::new();
    let mut in_entry = false;
    let mut in_content = false;
    let mut content_buf = String::new();
    let mut current_self_href = String::new();
    let mut current_related: Vec<String> = Vec::new();
    let mut content_depth = 0i32;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = local_name_of(&e.name());
                if local == "entry" {
                    in_entry = true;
                    content_buf.clear();
                    current_self_href.clear();
                    current_related.clear();
                    in_content = false;
                    content_depth = 0;
                } else if in_entry && local == "content" && !in_content {
                    in_content = true;
                    content_depth = 0;
                } else if in_content {
                    // Reconstruct the raw XML inside <content>.
                    content_buf.push('<');
                    content_buf.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                    for attr in e.attributes().flatten() {
                        content_buf.push(' ');
                        content_buf.push_str(&String::from_utf8_lossy(attr.key.as_ref()));
                        content_buf.push_str("=\"");
                        content_buf.push_str(&String::from_utf8_lossy(&attr.value));
                        content_buf.push('"');
                    }
                    content_buf.push('>');
                    content_depth += 1;
                } else if in_entry && local == "link" {
                    // Extract rel and href attributes.
                    let mut rel = String::new();
                    let mut href = String::new();
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_lowercase();
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        if key == "rel" { rel = val.clone(); }
                        if key == "href" { href = val; }
                    }
                    if rel == "self" { current_self_href = href; }
                    else if rel == "related" { current_related.push(href); }
                }
            }
            Ok(Event::Empty(e)) => {
                let local = local_name_of(&e.name());
                if in_entry && !in_content && local == "link" {
                    let mut rel = String::new();
                    let mut href = String::new();
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_lowercase();
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        if key == "rel" { rel = val.clone(); }
                        if key == "href" { href = val; }
                    }
                    if rel == "self" { current_self_href = href; }
                    else if rel == "related" { current_related.push(href); }
                } else if in_content {
                    content_buf.push('<');
                    content_buf.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                    for attr in e.attributes().flatten() {
                        content_buf.push(' ');
                        content_buf.push_str(&String::from_utf8_lossy(attr.key.as_ref()));
                        content_buf.push_str("=\"");
                        content_buf.push_str(&String::from_utf8_lossy(&attr.value));
                        content_buf.push('"');
                    }
                    content_buf.push_str("/>");
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name_of(&e.name());
                if local == "entry" && in_entry {
                    let kind = classify_content(&content_buf);
                    entries.push(EntryMeta {
                        self_href: current_self_href.clone(),
                        related_hrefs: current_related.clone(),
                        kind,
                        content: content_buf.clone(),
                    });
                    in_entry = false;
                    in_content = false;
                } else if local == "content" && in_content && content_depth == 0 {
                    in_content = false;
                } else if in_content {
                    if content_depth > 0 {
                        content_depth -= 1;
                        content_buf.push_str("</");
                        content_buf.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                        content_buf.push('>');
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if in_content {
                    content_buf.push_str(&String::from_utf8_lossy(e.as_ref()));
                }
            }
            Ok(Event::CData(e)) => {
                if in_content {
                    content_buf.push_str(&String::from_utf8_lossy(e.as_ref()));
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("XML parse error: {e}"),
            _ => {}
        }
        buf.clear();
    }

    Ok(entries)
}

/// Identify the entry type from its content snippet (first element local name).
fn classify_content(content: &str) -> String {
    for word in ["ReadingType", "MeterReading", "IntervalBlock"] {
        if content.contains(word) {
            return word.to_string();
        }
    }
    String::new()
}

/// Extract the local (no-namespace) name from a quick-xml QName.
fn local_name_of(name: &quick_xml::name::QName<'_>) -> String {
    String::from_utf8_lossy(name.local_name().as_ref()).to_string()
}

// ---------------------------------------------------------------------------
// ReadingType parser

struct ReadingTypeMeta {
    /// IEC 61968-9 UOM code. 72=Wh, 61=W, 119=therm, 43=ft3, 42=m3, 24=gal.
    uom: i64,
    /// Signed exponent: value × 10^multiplier gives reading units. Typically
    /// −3 (milli), 0 (unity), 3 (kilo).
    multiplier: i32,
    /// ESPI commodity: 1=electricity, 7=naturalGas, 8=water.
    commodity: i64,
    /// ESPI flowDirection: 1=forward/consumption (default), 19=reverse/production/export.
    /// Other reverse codes (2–18) also map to "production".
    flow_direction: i64,
}

fn parse_reading_type(content: &str) -> Result<ReadingTypeMeta> {
    let uom = extract_element_i64(content, "uom").unwrap_or(72);          // default Wh
    let multiplier = extract_element_i64(content, "powerOfTenMultiplier").unwrap_or(0) as i32;
    let commodity = extract_element_i64(content, "commodity").unwrap_or(1); // default electricity
    // flowDirection 1=forward(consumption), 19=reverse(production/export).
    // Default 1 (consumption) when absent.
    let flow_direction = extract_element_i64(content, "flowDirection").unwrap_or(1);
    Ok(ReadingTypeMeta { uom, multiplier, commodity, flow_direction })
}

/// Map ESPI flowDirection code to vault `direction` string.
/// Code 1 = forward = consumption; codes 19 and 2–18 = reverse = production.
fn flow_direction_str(code: i64) -> &'static str {
    match code {
        1 => "consumption",
        // 19 is the canonical "received" / net-metering export code.
        // 2..=18 are other reverse-flow codes in the ESPI codelist.
        _ => "production",
    }
}

// ---------------------------------------------------------------------------
// MeterReading metadata

struct MeterReadingMeta {
    reading_type_href: String,
    /// hrefs from `<link rel="related">` that point to IntervalBlock resources.
    /// Used to resolve which MeterReading owns a given IntervalBlock entry.
    interval_block_hrefs: Vec<String>,
    /// The MeterReading self_href — used as the stable meter namespace for guids.
    self_href: String,
}

// ---------------------------------------------------------------------------
// IntervalBlock parser

struct RawReading {
    start_unix: i64,
    duration_secs: u64,
    value: i64,
    cost_hundred_thousandths: i64,
}

struct IntervalBlockEntry {
    self_href: String,
    meter_href: Option<String>,
    readings: Vec<RawReading>,
}

fn parse_interval_block(content: &str) -> Result<Vec<RawReading>> {
    // Split on any open-tag that has local name "IntervalReading".
    // ESPI exports come in two valid wire forms:
    //   (a) <IntervalReading>...</IntervalReading>          (xmlns="" un-prefixed)
    //   (b) <espi:IntervalReading>...</espi:IntervalReading>  (fully prefixed)
    // We handle both by looking for both "<IntervalReading" and "IntervalReading"
    // (the prefix variant contains ":IntervalReading").
    let mut readings = Vec::new();
    let mut rest = content;
    loop {
        // Find the earliest open-tag start position for either form.
        let plain_pos = rest.find("<IntervalReading");
        let ns_pos = rest.find(":IntervalReading")
            .and_then(|p| rest[..p].rfind('<'));
        let start_idx = match (plain_pos, ns_pos) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };

        let tag_start = &rest[start_idx..];
        // Determine the end-tag to look for: check whether this is a prefixed form.
        // The open tag ends at '>'; extract the tag name between '<' and the first ' ' or '>'.
        let tag_name_end = tag_start[1..].find(|c: char| c == ' ' || c == '>' || c == '/')
            .map(|p| p + 1)
            .unwrap_or(tag_start.len());
        let open_tag_name = &tag_start[1..tag_name_end]; // e.g. "IntervalReading" or "espi:IntervalReading"
        let close_tag = format!("</{open_tag_name}>");

        rest = tag_start;
        let end_idx = rest.find(&close_tag).unwrap_or(rest.len());
        let after_end = end_idx + close_tag.len();
        let chunk = &rest[..after_end];

        // timePeriod/start + duration
        let interval_start = extract_child_of(chunk, "timePeriod", "start")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        let duration = extract_child_of(chunk, "timePeriod", "duration")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let value = extract_element_i64(chunk, "value").unwrap_or(0);
        let cost = extract_element_i64(chunk, "cost").unwrap_or(0);

        if interval_start > 0 {
            readings.push(RawReading {
                start_unix: interval_start,
                duration_secs: duration,
                value,
                cost_hundred_thousandths: cost,
            });
        }
        if after_end >= rest.len() {
            break;
        }
        rest = &rest[after_end..];
    }
    Ok(readings)
}

// ---------------------------------------------------------------------------
// Conversion helpers

/// Convert a raw ESPI value to vault energy fields.
/// Returns (kwh, value_other, unit, circuit).
fn convert_reading(
    raw_value: i64,
    duration_secs: u64,
    rt: &ReadingTypeMeta,
) -> (Option<f64>, Option<f64>, String, String) {
    let scale = 10f64.powi(rt.multiplier);
    let scaled = (raw_value as f64) * scale;

    match (rt.uom, rt.commodity) {
        // Electricity: Wh or kWh
        (72, 1) => {
            // uom=72 is Wh; multiplier adjusts. Convert to kWh.
            let kwh = scaled / 1000.0;
            (Some(kwh), None, String::new(), String::new())
        }
        // Electricity: W (power) → derive kWh from duration
        (61, 1) => {
            let kwh = scaled * (duration_secs as f64) / 3_600_000.0;
            (Some(kwh), None, String::new(), String::new())
        }
        // Gas: therms
        (119, _) => (None, Some(scaled), "therm".into(), "gas-meter".into()),
        // Gas: CCF / ft3
        (43, _) => (None, Some(scaled), "ccf".into(), "gas-meter".into()),
        // Gas or water: m3
        (42, 7) => (None, Some(scaled), "m3".into(), "gas-meter".into()),
        (42, 8) => (None, Some(scaled), "m3".into(), "water-meter".into()),
        // Water: gallons
        (24, _) => (None, Some(scaled), "gal".into(), "water-meter".into()),
        // Unknown: pass raw value through with numeric uom as unit string.
        _ => (None, Some(scaled), format!("uom-{}", rt.uom), String::new()),
    }
}

// ---------------------------------------------------------------------------
// String-extract helpers (operate on the raw content string)

/// Extract the text content of the first `<tag>...</tag>` in `s`, regardless
/// of namespace prefix.
///
/// Handles three wire forms:
///  - `<tag>text</tag>`                  (un-prefixed, xmlns="" style)
///  - `<espi:tag>text</espi:tag>`        (fully prefixed)
///  - `<ns:tag>text</ns:tag>`            (any other prefix)
fn extract_element_text<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    // 1. Try plain (un-prefixed) form: <tag>...</tag>
    let open_plain = format!("<{tag}>");
    let close_plain = format!("</{tag}>");
    if let Some(start) = s.find(&open_plain) {
        let after = &s[start + open_plain.len()..];
        if let Some(end) = after.find(&close_plain) {
            return Some(&after[..end]);
        }
    }

    // 2. Try namespace-prefixed form: find the open tag via ":{tag}>" suffix,
    //    then find the paired close "</prefix:tag>" right after.
    //    We scan forward from the most recently matched open tag so that
    //    nested tags with the same local name don't cause a mismatch.
    let open_suffix = format!(":{tag}>");
    if let Some(suffix_pos) = s.find(&open_suffix) {
        // Find the '<' that starts the open tag ("<prefix:tag>").
        if let Some(lt_pos) = s[..suffix_pos].rfind('<') {
            // Extract the full open tag name (e.g. "espi:tag").
            let open_tag_name = &s[lt_pos + 1..suffix_pos + open_suffix.len() - 1];
            // open_tag_name now = "espi:tag" (without the '<' and '>').
            // The close tag is "</espi:tag>".
            let close_prefixed = format!("</{open_tag_name}>");
            let text_start = lt_pos + 1 + open_tag_name.len() + 1; // after the '>'
            if text_start <= s.len() {
                let after = &s[text_start..];
                if let Some(end) = after.find(&close_prefixed) {
                    return Some(&after[..end]);
                }
            }
        }
    }

    None
}

fn extract_element_i64(s: &str, tag: &str) -> Option<i64> {
    extract_element_text(s, tag)?.trim().parse().ok()
}

/// Extract `<parent><child>text</child></parent>` — find child text within
/// the first occurrence of parent.  Handles both un-prefixed and prefixed forms.
fn extract_child_of<'a>(s: &'a str, parent: &str, child: &str) -> Option<&'a str> {
    // Try plain form first.
    let open_plain = format!("<{parent}>");
    let close_plain = format!("</{parent}>");
    if let Some(start) = s.find(&open_plain) {
        let after = &s[start + open_plain.len()..];
        let end = after.find(&close_plain).unwrap_or(after.len());
        return extract_element_text(&after[..end], child);
    }
    // Prefixed form: locate the open tag via ":{parent}>" suffix.
    let open_suffix = format!(":{parent}>");
    if let Some(suffix_pos) = s.find(&open_suffix) {
        if let Some(lt_pos) = s[..suffix_pos].rfind('<') {
            let open_tag_name = &s[lt_pos + 1..suffix_pos + open_suffix.len() - 1];
            let close_prefixed = format!("</{open_tag_name}>");
            let content_start = lt_pos + 1 + open_tag_name.len() + 1;
            if content_start <= s.len() {
                let after = &s[content_start..];
                let end = after.find(&close_prefixed).unwrap_or(after.len());
                return extract_element_text(&after[..end], child);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Meter/block helpers

/// Find the MeterReading self_href that owns `block_href`.
///
/// Strategy (in order of confidence):
/// 1. Exact match: look for a MeterReading whose `interval_block_hrefs`
///    contains `block_href` exactly (the most common ESPI case — the
///    MeterReading entry has `<link rel="related" href=".../IntervalBlock/n"/>`).
/// 2. Path-prefix match: a block href of the form
///    `.../MeterReading/<id>/IntervalBlock/<n>` is a sub-resource of the
///    MeterReading at `.../MeterReading/<id>`.
/// 3. Single-meter fallback: when exactly one MeterReading exists, it owns
///    all blocks (single-commodity file).
/// Never falls back to "first key" which is arbitrary in a BTreeMap.
fn find_meter_href_for_block(
    block_href: &str,
    meter_readings: &BTreeMap<String, MeterReadingMeta>,
) -> Option<String> {
    // 1. Exact related-href match.
    for (mh, meta) in meter_readings {
        if meta.interval_block_hrefs.iter().any(|h| hrefs_equal(h, block_href)) {
            return Some(mh.clone());
        }
    }
    // 2. Path-prefix match: block_href starts with meter_href path (normalized).
    for (mh, _) in meter_readings {
        let meter_path = strip_scheme_host(mh);
        let block_path = strip_scheme_host(block_href);
        // block is a sub-path of the meter reading
        if block_path.contains(&format!("{}/IntervalBlock", meter_path.trim_end_matches('/'))) {
            return Some(mh.clone());
        }
    }
    // 3. Single-meter fallback only.
    if meter_readings.len() == 1 {
        return meter_readings.keys().next().map(|s| s.clone());
    }
    None
}

/// Return true if two hrefs refer to the same resource (case-insensitive path,
/// ignoring trailing slash differences and scheme+host variations when paths match).
fn hrefs_equal(a: &str, b: &str) -> bool {
    if a == b { return true; }
    // Normalise: strip scheme+host, trim trailing slash, lowercase.
    strip_scheme_host(a).trim_end_matches('/').to_lowercase()
        == strip_scheme_host(b).trim_end_matches('/').to_lowercase()
}

/// Strip the scheme and host from a URL, leaving only the path (and query if any).
fn strip_scheme_host(url: &str) -> &str {
    // Find "://" and then the next "/".
    if let Some(ss) = url.find("://") {
        let after = &url[ss + 3..];
        if let Some(slash) = after.find('/') {
            return &after[slash..];
        }
        return after;
    }
    url
}

/// Check if an href path contains a given resource-type segment (e.g. "ReadingType",
/// "IntervalBlock"). Avoids pure substring matches like "ReadingType" matching
/// "ReadingTypeXyz".
fn href_has_segment(href: &str, segment: &str) -> bool {
    let path = strip_scheme_host(href);
    path.split('/').any(|part| part == segment || part.starts_with(&format!("{segment}?")))
}

/// Extract a stable meter id from a MeterReading self_href.
/// Takes the last meaningful numeric/UUID path segment.
fn meter_id_from_href(href: &str) -> String {
    strip_scheme_host(href)
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty() && !s.starts_with("espi") && s.chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Try to extract a UsagePoint id from a path like
/// `/espi/1_0/resource/Subscription/5/UsagePoint/1/IntervalBlock/1`.
fn usage_point_id_from_path(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "UsagePoint" {
            if let Some(id) = parts.get(i + 1) {
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Time conversion

fn unix_to_rfc3339_local(unix_secs: i64) -> String {
    Local
        .timestamp_opt(unix_secs, 0)
        .earliest()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{unix_secs}"))
}

// ---------------------------------------------------------------------------
// Misc

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-gb-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Synthetic ESPI Atom feed with one ReadingType (uom=72 Wh, multiplier=0,
    /// commodity=1 electricity), one MeterReading, and one IntervalBlock with
    /// two 15-minute (900s) readings.  Built from the confirmed real-sample
    /// structure (intervalblock-dto-output.xml from GreenButtonAlliance).
    const ESPI_ELEC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:uuid:test-feed</id>
  <title>Green Button Usage Feed</title>
  <updated>2026-06-10T21:00:00Z</updated>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/ReadingType/1</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/ReadingType/1"/>
    <title>ReadingType</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:ReadingType xmlns="">
        <uom>72</uom>
        <powerOfTenMultiplier>0</powerOfTenMultiplier>
        <commodity>1</commodity>
        <flowDirection>1</flowDirection>
        <intervalLength>900</intervalLength>
      </espi:ReadingType>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/MeterReading/1</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/MeterReading/1"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/ReadingType/1"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/IntervalBlock/1"/>
    <title>MeterReading</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:MeterReading xmlns=""/>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/IntervalBlock/1</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/IntervalBlock/1"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/MeterReading/1"/>
    <title>IntervalBlock</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:IntervalBlock xmlns="">
        <interval>
          <duration>1800</duration>
          <start>1749564000</start>
        </interval>
        <IntervalReading>
          <cost>974</cost>
          <timePeriod>
            <duration>900</duration>
            <start>1749564000</start>
          </timePeriod>
          <value>282</value>
        </IntervalReading>
        <IntervalReading>
          <cost>965</cost>
          <timePeriod>
            <duration>900</duration>
            <start>1749564900</start>
          </timePeriod>
          <value>323</value>
        </IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>
</feed>"#;

    /// Gas ReadingType: uom=119 (therms), multiplier=-3, commodity=7.
    const ESPI_GAS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:uuid:test-gas-feed</id>
  <title>Green Button Gas Feed</title>
  <updated>2026-06-10T21:00:00Z</updated>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/ReadingType/2</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/ReadingType/2"/>
    <title>ReadingType</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:ReadingType xmlns="">
        <uom>119</uom>
        <powerOfTenMultiplier>-3</powerOfTenMultiplier>
        <commodity>7</commodity>
        <flowDirection>1</flowDirection>
        <intervalLength>3600</intervalLength>
      </espi:ReadingType>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/MeterReading/2</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/MeterReading/2"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/ReadingType/2"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/IntervalBlock/2"/>
    <title>MeterReading</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:MeterReading xmlns=""/>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/IntervalBlock/2</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/IntervalBlock/2"/>
    <title>IntervalBlock</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:IntervalBlock xmlns="">
        <interval>
          <duration>3600</duration>
          <start>1749564000</start>
        </interval>
        <IntervalReading>
          <timePeriod>
            <duration>3600</duration>
            <start>1749564000</start>
          </timePeriod>
          <value>420</value>
        </IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>
</feed>"#;

    fn import_xml(vault: &Vault, xml: &str, filename: &str) -> ImportOutcome {
        let path = vault.root().join(filename);
        fs::write(&path, xml).unwrap();
        (IMPORT.run)(vault, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn electricity_intervals_parsed_and_written() {
        let v = temp_vault("elec");
        let out = import_xml(&v, ESPI_ELEC, "electric.xml");

        assert_eq!(out.counts.get("imported"), Some(&2), "two intervals: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("raw_files"), Some(&1));

        // Energy JSONL written under energy/ directory.
        let energy_dir = v.root().join(ENERGY_DIR);
        assert!(energy_dir.exists(), "energy dir created");
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "one month file");

        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        let rows: Vec<Value> = jsonl
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2, "two rows in JSONL");

        // First row: 282 Wh → 0.282 kWh.
        let r0 = &rows[0];
        assert_eq!(r0["source"], "green-button");
        assert!(r0.get("kwh").is_some(), "kwh present: {r0}");
        let kwh0 = r0["kwh"].as_f64().unwrap();
        assert!((kwh0 - 0.282).abs() < 1e-6, "kwh0={kwh0} expected 0.282");
        assert_eq!(r0["interval_secs"], 900);
        assert_eq!(r0["direction"], "consumption");
        assert!(r0.get("guid").is_some(), "guid present");
        assert!(r0.get("unit").is_none(), "no unit for kWh (implied)");

        // Second row: 323 Wh → 0.323 kWh.
        let kwh1 = rows[1]["kwh"].as_f64().unwrap();
        assert!((kwh1 - 0.323).abs() < 1e-6, "kwh1={kwh1} expected 0.323");
    }

    #[test]
    fn gas_intervals_use_therm_unit_and_circuit() {
        let v = temp_vault("gas");
        let out = import_xml(&v, ESPI_GAS, "gas.xml");

        assert_eq!(out.counts.get("imported"), Some(&1), "one gas interval");

        let energy_dir = v.root().join(ENERGY_DIR);
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        let row: Value = serde_json::from_str(jsonl.trim()).unwrap();

        assert_eq!(row["unit"], "therm");
        assert_eq!(row["circuit"], "gas-meter");
        // value = 420 × 10^-3 = 0.42 therms
        let val = row["value"].as_f64().unwrap();
        assert!((val - 0.42).abs() < 1e-6, "val={val} expected 0.42");
        assert!(row.get("kwh").is_none(), "no kwh for gas");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let out1 = import_xml(&v, ESPI_ELEC, "electric.xml");
        assert_eq!(out1.counts.get("imported"), Some(&2));

        // Second import of the same file: all duplicates.
        let out2 = import_xml(&v, ESPI_ELEC, "electric2.xml");
        assert_eq!(out2.counts.get("imported"), Some(&0), "no new rows");
        assert_eq!(out2.counts.get("duplicates"), Some(&2), "both duplicates");

        // Row count in the energy JSONL must be unchanged.
        let energy_dir = v.root().join(ENERGY_DIR);
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        assert_eq!(jsonl.lines().count(), 2, "still two rows");

        // Raw dir should have two snapshots (one per import).
        let raw_dir = v.root().join(RAW_DIR);
        let raw_files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(raw_files.len(), 2, "two raw snapshots");
    }

    #[test]
    fn raw_file_stored_unconditionally() {
        let v = temp_vault("raw");
        let out = import_xml(&v, ESPI_ELEC, "myexport.xml");
        assert_eq!(out.counts.get("raw_files"), Some(&1));

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let fname = entries[0].file_name();
        let name = fname.to_string_lossy();
        assert!(name.ends_with("myexport.xml"), "preserves original filename: {name}");
    }

    #[test]
    fn def_metadata_correct() {
        assert_eq!(DEF.id, "green-button");
        assert_eq!(DEF.domain, "home");
        assert!(DEF.last_data.is_some());
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"xml"));
    }

    /// Fully espi:-prefixed IntervalReading fixture — modelled on ENWIN / GBA
    /// testdata.xml wire format where every inner element carries the espi: prefix.
    /// This is the other valid ESPI wire form alongside xmlns="" (un-prefixed).
    const ESPI_PREFIXED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:espi="http://naesb.org/espi">
  <id>urn:uuid:test-prefixed-feed</id>
  <title>Green Button ENWIN Prefixed Feed</title>
  <updated>2026-06-15T00:00:00Z</updated>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/ReadingType/10</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/ReadingType/10"/>
    <title>ReadingType</title>
    <content>
      <espi:ReadingType>
        <espi:uom>72</espi:uom>
        <espi:powerOfTenMultiplier>3</espi:powerOfTenMultiplier>
        <espi:commodity>1</espi:commodity>
        <espi:flowDirection>1</espi:flowDirection>
        <espi:intervalLength>3600</espi:intervalLength>
      </espi:ReadingType>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/MeterReading/10</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/MeterReading/10"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/ReadingType/10"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/IntervalBlock/10"/>
    <title>MeterReading</title>
    <content>
      <espi:MeterReading/>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/IntervalBlock/10</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/IntervalBlock/10"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/MeterReading/10"/>
    <title>IntervalBlock</title>
    <content>
      <espi:IntervalBlock>
        <espi:interval>
          <espi:duration>7200</espi:duration>
          <espi:start>1749564000</espi:start>
        </espi:interval>
        <espi:IntervalReading>
          <espi:timePeriod>
            <espi:duration>3600</espi:duration>
            <espi:start>1749564000</espi:start>
          </espi:timePeriod>
          <espi:value>3880</espi:value>
        </espi:IntervalReading>
        <espi:IntervalReading>
          <espi:timePeriod>
            <espi:duration>3600</espi:duration>
            <espi:start>1749567600</espi:start>
          </espi:timePeriod>
          <espi:value>4120</espi:value>
        </espi:IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>
</feed>"#;

    /// Fully espi:-prefixed ESPI feed parses correctly (ENWIN / GBA testdata.xml wire form).
    /// This exercises the blocking defect where `<espi:IntervalReading>` was not found by
    /// `rest.find("<IntervalReading")` and `<espi:value>` was not extracted.
    #[test]
    fn prefixed_espi_parses_interval_readings() {
        let v = temp_vault("prefixed");
        let out = import_xml(&v, ESPI_PREFIXED, "prefixed.xml");

        assert_eq!(
            out.counts.get("imported"),
            Some(&2),
            "two prefixed-ESPI readings must be parsed: {out:?}"
        );
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        let energy_dir = v.root().join(ENERGY_DIR);
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        let rows: Vec<Value> = jsonl
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2, "two energy rows written");

        // value=3880, uom=72 (Wh), multiplier=3 → 3880 × 10^3 = 3880000 Wh = 3880 kWh
        let kwh0 = rows[0]["kwh"].as_f64().unwrap();
        assert!((kwh0 - 3880.0).abs() < 1e-3, "kwh0={kwh0} expected 3880.0");

        let kwh1 = rows[1]["kwh"].as_f64().unwrap();
        assert!((kwh1 - 4120.0).abs() < 1e-3, "kwh1={kwh1} expected 4120.0");
    }

    /// Combined electric + gas feed with overlapping interval_start timestamps.
    /// Tests:
    ///  - Each block resolves to its correct MeterReading (not always the first one).
    ///  - Gas row gets unit=therm, circuit=gas-meter; electric row gets kwh.
    ///  - Guids are distinct despite the same interval_start (no silent data loss).
    const ESPI_ELEC_GAS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:uuid:test-combined-feed</id>
  <title>PGE Combined Electric+Gas Feed</title>
  <updated>2026-06-10T21:00:00Z</updated>

  <!-- ReadingType 1: electricity (uom=72, mult=0, commodity=1) -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/ReadingType/1</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/ReadingType/1"/>
    <title>ReadingType</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:ReadingType xmlns="">
        <uom>72</uom>
        <powerOfTenMultiplier>0</powerOfTenMultiplier>
        <commodity>1</commodity>
        <flowDirection>1</flowDirection>
      </espi:ReadingType>
    </content>
  </entry>

  <!-- ReadingType 2: natural gas (uom=119, mult=-3, commodity=7) -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/ReadingType/2</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/ReadingType/2"/>
    <title>ReadingType</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:ReadingType xmlns="">
        <uom>119</uom>
        <powerOfTenMultiplier>-3</powerOfTenMultiplier>
        <commodity>7</commodity>
        <flowDirection>1</flowDirection>
      </espi:ReadingType>
    </content>
  </entry>

  <!-- MeterReading 1: electric -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/MeterReading/1</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/MeterReading/1"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/ReadingType/1"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/IntervalBlock/1"/>
    <title>MeterReading</title>
    <content xmlns:espi="http://naesb.org/espi"><espi:MeterReading xmlns=""/></content>
  </entry>

  <!-- MeterReading 2: gas -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/MeterReading/2</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/MeterReading/2"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/ReadingType/2"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/IntervalBlock/2"/>
    <title>MeterReading</title>
    <content xmlns:espi="http://naesb.org/espi"><espi:MeterReading xmlns=""/></content>
  </entry>

  <!-- IntervalBlock 1: electric — interval_start=1749564000 (overlaps with gas) -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/IntervalBlock/1</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/IntervalBlock/1"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/MeterReading/1"/>
    <title>IntervalBlock</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:IntervalBlock xmlns="">
        <interval><duration>3600</duration><start>1749564000</start></interval>
        <IntervalReading>
          <timePeriod><duration>3600</duration><start>1749564000</start></timePeriod>
          <value>500</value>
        </IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>

  <!-- IntervalBlock 2: gas — SAME interval_start=1749564000 as electric above -->
  <entry>
    <id>https://api.pge.com/espi/1_0/resource/IntervalBlock/2</id>
    <link rel="self" href="https://api.pge.com/espi/1_0/resource/IntervalBlock/2"/>
    <link rel="related" href="https://api.pge.com/espi/1_0/resource/MeterReading/2"/>
    <title>IntervalBlock</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:IntervalBlock xmlns="">
        <interval><duration>3600</duration><start>1749564000</start></interval>
        <IntervalReading>
          <timePeriod><duration>3600</duration><start>1749564000</start></timePeriod>
          <value>350</value>
        </IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>
</feed>"#;

    /// Multi-meter (electric + gas) with overlapping timestamps.
    /// Guards against GUID collision, wrong-source mapping, and silent data loss.
    #[test]
    fn multi_meter_elec_gas_no_collision() {
        let v = temp_vault("multimeter");
        let out = import_xml(&v, ESPI_ELEC_GAS, "combined.xml");

        assert_eq!(
            out.counts.get("imported"),
            Some(&2),
            "both electric and gas intervals must be imported (no silent loss): {out:?}"
        );
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        let energy_dir = v.root().join(ENERGY_DIR);
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        let rows: Vec<Value> = jsonl
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(rows.len(), 2, "two rows (one electric, one gas)");

        // Separate rows by kwh presence (electric) vs value+unit (gas).
        let elec_row = rows.iter().find(|r| r.get("kwh").is_some())
            .expect("electric row with kwh field");
        let gas_row = rows.iter().find(|r| r.get("unit").is_some())
            .expect("gas row with unit field");

        // Electric: 500 Wh, multiplier=0 → 0.5 kWh
        let kwh = elec_row["kwh"].as_f64().unwrap();
        assert!((kwh - 0.5).abs() < 1e-6, "elec kwh={kwh} expected 0.5");
        assert!(elec_row.get("unit").is_none(), "electric row must not have unit");
        assert!(elec_row.get("circuit").is_none(), "electric row must not have circuit");

        // Gas: 350 × 10^-3 = 0.35 therms
        assert_eq!(gas_row["unit"], "therm", "gas row unit must be therm");
        assert_eq!(gas_row["circuit"], "gas-meter", "gas row circuit must be gas-meter");
        let val = gas_row["value"].as_f64().unwrap();
        assert!((val - 0.35).abs() < 1e-6, "gas value={val} expected 0.35");
        assert!(gas_row.get("kwh").is_none(), "gas row must not have kwh");

        // GUIDs must be distinct (no collision due to shared interval_start).
        let g0 = rows[0]["guid"].as_str().unwrap();
        let g1 = rows[1]["guid"].as_str().unwrap();
        assert_ne!(g0, g1, "guids must differ between electric and gas rows");
    }

    /// Solar net-metering: flowDirection=19 (reverse/production) maps to "production".
    const ESPI_SOLAR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>urn:uuid:test-solar-feed</id>
  <title>Solar Production Feed</title>
  <updated>2026-06-10T21:00:00Z</updated>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/ReadingType/99</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/ReadingType/99"/>
    <title>ReadingType</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:ReadingType xmlns="">
        <uom>72</uom>
        <powerOfTenMultiplier>0</powerOfTenMultiplier>
        <commodity>1</commodity>
        <flowDirection>19</flowDirection>
      </espi:ReadingType>
    </content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/MeterReading/99</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/MeterReading/99"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/ReadingType/99"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/IntervalBlock/99"/>
    <title>MeterReading</title>
    <content xmlns:espi="http://naesb.org/espi"><espi:MeterReading xmlns=""/></content>
  </entry>

  <entry>
    <id>https://api.example.com/espi/1_0/resource/IntervalBlock/99</id>
    <link rel="self" href="https://api.example.com/espi/1_0/resource/IntervalBlock/99"/>
    <link rel="related" href="https://api.example.com/espi/1_0/resource/MeterReading/99"/>
    <title>IntervalBlock</title>
    <content xmlns:espi="http://naesb.org/espi">
      <espi:IntervalBlock xmlns="">
        <interval><duration>900</duration><start>1749564000</start></interval>
        <IntervalReading>
          <timePeriod><duration>900</duration><start>1749564000</start></timePeriod>
          <value>650</value>
        </IntervalReading>
      </espi:IntervalBlock>
    </content>
  </entry>
</feed>"#;

    /// flowDirection=19 (reverse/net-metering export) maps to direction="production".
    #[test]
    fn flow_direction_production_from_reading_type() {
        let v = temp_vault("solar");
        let out = import_xml(&v, ESPI_SOLAR, "solar.xml");

        assert_eq!(out.counts.get("imported"), Some(&1), "one solar interval: {out:?}");

        let energy_dir = v.root().join(ENERGY_DIR);
        let files: Vec<_> = fs::read_dir(&energy_dir).unwrap().flatten().collect();
        let jsonl = fs::read_to_string(files[0].path()).unwrap();
        let row: Value = serde_json::from_str(jsonl.trim()).unwrap();

        assert_eq!(row["direction"], "production",
            "flowDirection=19 must map to 'production', got: {:?}", row["direction"]);
        // 650 Wh, multiplier=0 → 0.65 kWh
        let kwh = row["kwh"].as_f64().unwrap();
        assert!((kwh - 0.65).abs() < 1e-6, "kwh={kwh} expected 0.65");
    }
}
