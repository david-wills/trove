# Utility Smart Meter (Green Button)

- **id:** `green-button`
- **domains:** `home/` (contract: **Phase 3 pending** — home readings drafted
  from HomeKit + Hue + Tempest + Enphase + Green Button together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (manual download from utility account → drop in app)
- **connection:** none — the user logs into their utility's website
  themselves; Trove only parses the file. (Green Button *Connect My Data*
  API exists but is supported by fewer than 55 utilities — not the primary
  path.)
- **evidence:** official-standard — ESPI XML is an open standard; Download
  My Data supported by most major US utilities (PG&E, ConEd, ComEd, …);
  per research doc
- **effort / priority:** M / P2
- **needs:** home contract not yet ratified (Needs-David) — raw layer can
  ship first

## What it is

Green Button is the US-standard export of utility smart-meter data: hourly
or 15-minute electricity usage (kWh), often gas, occasionally water. It is
the only path to whole-household energy history for anyone without a
dedicated monitor (Sense, Emporia), and it works for renters. The user
clicks "Download My Data" on their utility's site and imports the XML.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Electricity usage | any utility w/ smart meter + DMD | interval ts, duration, kWh | ESPI open standard |
| Gas usage | utility-dependent (often bundled) | interval ts, therms/CCF | ESPI open standard |
| Water usage | rare (some utilities bundle all) | interval ts, gallons | research doc note |
| Utility CSV variants | utility-specific (PG&E exports both) | same readings, CSV-shaped | research doc note |

All optional in the contract; an electricity-only export simply carries no
gas rows. No utility-specific code paths in the contract layer.

## Access & auth

- Manual: utility website → account → energy usage → "Download My Data" →
  XML file (ESPI format), typically named `GreenButton_*.xml` or
  `usage.xml`. Some utilities export CSV instead.
- CMD (automated API) requires per-utility registration and is sparsely
  adopted — out of scope for v1; the manual importer covers the vast
  majority of US users.
- No auth, no TCC, no network. Standalone-clean: pure file parse
  (`quick_xml` in Rust).

## Vault mapping

- **Raw layer:** `home/green-button/raw/` — the imported XML/CSV files
  kept verbatim (imports are cheap to keep; re-parse on contract changes).
- **Contract layer:** `home/green-button/YYYY-MM.jsonl` per the (pending)
  home readings contract — one row per meter interval (`ts`,
  `duration_secs`, `kind` = electricity/gas/water, `value`, `unit`,
  meter/account id), overflow in `extra`.
- **Dedupe:** `guid` from meter id + interval start — re-importing an
  overlapping export must be idempotent (utilities export rolling windows).

## Build plan

1. Module `crates/trove-core/src/green_button.rs`: `DEF` with
   `Behavior::Import` — registry-driven import box, no connection.
2. ESPI XML parser (`quick_xml`): `UsagePoint`/`MeterReading`/
   `IntervalBlock` → interval rows; unit + multiplier handling per the
   standard.
3. Utility CSV variants are **parser-last, Needs-sample**: ship ESPI XML
   first; add per-utility CSV parsers (PG&E first) only against real
   sample files.
4. Fixtures: a synthetic ESPI file built from the published schema, plus
   overlap-reimport idempotency test; unique temp dirs.
5. Raw layer can ship immediately; contract rows land once the home
   contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| ESPI XML import (un-prefixed, xmlns="" form) | ✅ built + tested | cargo test -p trove-core green_button:: passes 8 tests |
| ESPI XML import (fully espi:-prefixed form, ENWIN/GBA testdata.xml) | ✅ built + tested | `prefixed_espi_parses_interval_readings` — 2 rows from `<espi:IntervalReading>/<espi:value>` fixture |
| Multi-meter (electric+gas) with overlapping timestamps | ✅ built + tested | `multi_meter_elec_gas_no_collision` — guids distinct, gas→therm+gas-meter, electric→kwh, no silent dedup loss |
| Solar/net-metering (flowDirection=19 → production) | ✅ built + tested | `flow_direction_production_from_reading_type` — direction="production" from ReadingType.flowDirection |
| Re-import idempotency | ✅ built + tested | `reimport_is_idempotent` — row count unchanged, two raw snapshots |
| Utility CSV variants | ❌ Needs-sample | CSV import returns a clear error pointing the user to the raw file; add per-utility CSV parsers only against real samples |

## Build notes (2026-06-21 + fix 2026-06-21)

- Behavior: `Import` (accepts `.xml`)
- Contract mode: `deferred-sibling-draft` (`home.energy`) — the ESPI energy
  interval shape matches `home.energy` in `docs/vault-spec/domains/home.md`
  exactly (the example literally names `green-button`). The Rust struct
  for `home.energy` is unbound (only `HomeReading` is bound in `home.rs`).
  Rows are written as ad-hoc `serde_json::Value` to
  `home/green-button/energy/YYYY-MM.jsonl` following the spec schema.
- Raw layer: verbatim XML copy under `home/green-button/raw/<stamp>-<name>`.
- ESPI XML wire forms: both are now supported — (a) inner elements un-prefixed
  via `xmlns=""` (GreenButtonAlliance intervalblock-dto-output.xml shape) and
  (b) fully espi:-prefixed (`<espi:IntervalReading>/<espi:value>` — ENWIN/GBA
  testdata.xml, Usage-Subscription-Feed-Anonymized.xml). `extract_element_text`
  and `extract_child_of` handle both by matching the paired close tag derived
  from the detected open-tag prefix.
- Multi-meter resolution: `MeterReadingMeta` now stores `interval_block_hrefs`;
  `find_meter_href_for_block` uses exact related-href match → path-prefix match
  → single-meter fallback (never arbitrary BTreeMap first-key). Guid includes
  commodity code so electric and gas at the same timestamp are distinct.
- flowDirection: `ReadingTypeMeta.flow_direction` parsed; code 1→consumption,
  all others (incl. 19 reverse/net-metering)→production.
- meter_id derived from MeterReading self_href (stable across re-exports), not
  from IntervalBlock self_href.
- `quick-xml 0.40.1` already in Cargo.toml — no new deps added.
- No connection, no new ConnectionDef, no new CONNECTIONS line.
- `touched_shared_contract_files`: false.

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Utility Smart
Meter (Green Button) (L1896–L1902). Feasibility 🟡 medium (parsing effort,
not access risk). DMD manual path is primary; CMD API adoption is sparse.
UtilityAPI.com aggregator considered and rejected (third-party cloud,
overkill for personal use). Research doc notes Green Button is the better
path for total household water than Moen Flo when the utility supports it.
