# Way of Life

- **id:** `way-of-life`
- **domains:** `habits/` (contract: **Phase 3 pending** — drafted from
  Habitica + Streaks + TickTick habits + Way of Life together)
- **status:** 🧪 scaffold (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (user-initiated CSV/Excel drop; no API, no automation)
- **connection:** none (file import — nothing to authenticate)
- **evidence:** official — in-app CSV/Excel export documented (Settings >
  Export); no public REST API; column layout not yet sampled
- **effort / priority:** S / P2
- **needs:** Needs-sample (export column layout undocumented — parser built
  last, against a real export) · habits contract not yet ratified
  (Needs-David)

## What it is

Way of Life is a long-running iOS/Android habit tracker (no Mac app): the
user marks each habit yes / no / skip per day and the app surfaces streaks
and chains. The data is a clean per-day boolean-ish log of personal
routines — valuable because it's an explicit self-asserted record of
behavior that nothing else captures. Niche but sticky among its users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Habit check-in log | free (export is in-app) | habit name, date, value (yes/no/skip) | official — Settings > Export |
| Excel export | free | same fields, xlsx layout | official |
| JSON export (Android only) | free | richer structure (Android variant) | research notes |

All optional in the contract (omit-if-empty). No tier gates the export
itself; there are no API-level capability tiers to model.

## Access & auth

- **No API, no webhooks.** The only path is the in-app export: Settings >
  Export → CSV or Excel (Android additionally offers JSON). The app's
  URL-scheme is for cross-app launching only, not data extraction.
- iOS/Android only — there is **no macOS app and no local DB on the Mac**
  to read. The user generates the export on their phone and drops the file
  into Trove.
- No auth, no TCC, no network. Standalone-clean by construction (it's a
  file the user hands over). Trove never talks to Way of Life servers.

## Vault mapping

- **Raw layer:** `habits/way-of-life/raw/<export-filename>` — the exact
  uploaded CSV/Excel preserved verbatim, plus a parsed
  `habits/way-of-life/raw/YYYY-MM.jsonl` of the row objects at full
  fidelity.
- **Contract layer:** `habits/way-of-life/YYYY-MM.jsonl` per the (pending)
  habits contract — expected shape: one row per habit-day check-in (`ts`
  = the check-in date, `source`, `guid` = stable hash of
  habit-name + date, `habit`, `value` one of yes/no/skip), anything the
  export carries beyond that in `extra`. Field mapping is provisional
  until both a real export sample and the ratified contract exist.
- **Dedupe:** `guid` = hash(habit name + date); re-importing an overlapping
  export is idempotent. No cursor (manual import; the import box owns
  re-run safety).

## Build plan

1. **Parser-last.** The CSV/Excel column layout is not documented — flag
   **Needs-sample** and build the parser against a real export rather than
   guessing headers. Until a sample lands, this provider is parked.
2. Module `crates/trove-core/src/way_of_life.rs`: `DEF` (Import behavior),
   import hook that accepts CSV and Excel (and Android JSON if a sample
   shows it's worth it); no `CONNECTION`.
3. Registration line in `INTEGRATIONS` only.
4. Fixtures from the real export sample (yes / no / skip rows, multiple
   habits, a date range); parser + store + dedupe-on-reimport tests, unique
   temp dirs. `letterboxd.rs` is the reference Import module.
5. Vault writes via `store` helpers once the habits contract is ratified;
   until then parked behind **Needs-David (contract)**.

## Build status (fan-out Phase B)

**Scaffold shipped** (`integration/way-of-life`). The module is complete with:
- `DEF` (Import behavior, accepts csv/xlsx, correct habits domain).
- Raw-layer write: uploaded file copied verbatim to `habits/way-of-life/raw/`.
- `habit_day_guid` (stable SHA-256 dedup key pre-defined for when parser lands).
- 8 tests green, `cargo check` clean.

**Parser parked (Needs-sample):** export column headers are undocumented;
no sample exists on disk. Drop a real Way of Life CSV export in
`crates/trove-core/tests/fixtures/way-of-life/` and wire the parser into
`run_import` — the module doc describes the exact steps.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | — | export a real CSV from the app, drop it in the import box, confirm rows in `habits/way-of-life/` + hub last-data |
| Excel import | — | repeat with an .xlsx export; confirm identical rows |
| Re-import idempotency | — | import an overlapping export twice; confirm no duplicate check-ins (guid dedupe) |

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity"
§Way of Life (Habit Tracker) (L2690–L2696). Feasibility 🟡 medium — export
exists but there is no API, so pulls are manual-only by design; this is a
genuine Import, not a parked Periodic. Last app update August 2025; no
shutdown signal. Habitica / Streaks / TickTick habits share the habits
domain — sequence one of them to exercise the contract with a second
source. Not privacy-sensitive (self-asserted routine names, no message
bodies / location / financial detail).
