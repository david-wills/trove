# Sense Energy Monitor

- **id:** `sense-energy`
- **domains:** `home/` (contract: **Phase 3 pending** — research doc sketches
  a shared energy-readings shape across Sense / Emporia / Enphase / Green
  Button / Kasa; the Phase 3 home contract decides it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (unofficial cloud API poll) + Import (web-app CSV
  export — the guaranteed path)
- **connection:** `sense-energy` — TokenPaste (Sense account email +
  password; **unofficial** API, label it as such in the connect card). Not
  shared with other defs. The CSV import path needs no connection.
- **evidence:** community-schema — unofficial reverse-engineered API via
  github.com/scottbonline/sense (works, could break on Sense's changes;
  confidence medium). Web-app CSV export is official and stable but its
  exact columns are undocumented → sample-required for the importer.
- **effort / priority:** M / P2
- **needs:** Needs-login (real account to spike/validate the unofficial
  API) · Needs-sample (CSV export columns) · privacy note: whole-home energy
  is mildly behavioral but not on the mandatory opt-in list

## What it is

Sense is a whole-home energy monitor that clips onto the electrical panel
and — its unique trick — disaggregates usage by appliance from electrical
signatures (always-on load, fridge, EV charger, etc.). Per-device usage
history with cost estimates is rich daily-life context no other source
provides. Sense has never shipped an official API despite years of
community requests.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Historical usage (CSV export) | none | hourly kWh | official web-app export (Usage screen → export); columns Needs-sample |
| Real-time whole-home + per-device | none (unofficial API) | watts now, detected-device states, WebSocket stream | community library, medium confidence |
| Detected devices + usage/cost | none (unofficial API) | device names, usage history, cost estimates | community library, medium confidence |

All optional in the contract; a CSV-only user gets hourly kWh rows and
nothing per-device — no special code paths.

## Access & auth

- **Unofficial API:** authenticate with Sense account credentials
  (reverse-engineered, per scottbonline/sense); REST for devices/usage +
  WebSocket for real-time. No published rate limits. Can break at any time
  — the def must degrade honestly (clear hub error, never silent).
- **CSV export:** Sense web app → Usage screen → export; manual but stable.
- No TCC, no local files; outbound HTTPS only. Standalone-clean (reimplement
  the protocol in Rust; never depend on the Python library at runtime).
- Credentials stored like other TokenPaste secrets; UI copy must state the
  integration is community-maintained/unofficial.

## Vault mapping

- **Raw layer:** `home/sense-energy/YYYY-MM.jsonl` — timestamped usage rows
  (whole-home and per-detected-device, `device` field distinguishing);
  imported CSV rows land in the same stream, full fidelity.
- **Contract layer:** home contract is **Phase 3 pending**; raw-only until
  ratified. The research doc's energy sketch ({ts, source, direction,
  watts_or_kwh, interval}) is input to that contract — the taxonomy routes
  it under `home/`, not a separate `energy/` folder.
- **Dedupe:** `guid` = device id (or `main`) + interval start timestamp;
  CSV imports and API polls converge. Cursor in
  `.trove/sense-energy-sync.json`, rebuildable.

## Build plan

1. **CSV importer first** (research doc: "start with M1 CSV import as
   guaranteed fallback") — but the export columns are undocumented, so the
   parser is **parser-last, Needs-sample**: obtain a real export before
   writing it.
2. **Spike the unofficial API** with a real account (Needs-login) before
   committing to the Periodic slice — verify auth flow + response shapes
   against the community library's documentation of them.
3. Module `crates/trove-core/src/sense_energy.rs` (def id `sense-energy`):
   `DEF` (Periodic + import hook), `CONNECTION` (TokenPaste email/password,
   setup copy flagging unofficial status), `pull` hook.
4. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
5. Fixtures from the community library's documented response shapes plus
   the sample CSV; parser + store + cursor tests, unique temp dirs.
6. Failure posture: on auth/API break, surface "Sense changed their
   unofficial API" on the card — honest, not silent.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | 🚧 parked | export hourly usage from the Sense web app, drop in import box — scaffold stores raw bytes; parser parked until exact columns confirmed (Needs-sample) |
| Unofficial API poll | 🧪 built | paste real credentials (email:password), Sync now; confirm whole-home + per-device rows in `home/sense-energy/energy/` and `home/sense-energy/raw/` (requires a Sense owner — Needs-login) |
| Real-time stream | — | deferred slice; validate only if the WebSocket path ships |

## Build notes (2026-06-21)

- **Behavior**: `Periodic` (hourly) using `GET app/monitors/{id}/history/usage?scale=DAY&start=...` — daily kWh per whole-home + per-detected-device.
- **Contract mode**: `deferred-sibling-draft` (home.energy) — raw JSONL + energy JSONL following emporia.rs pattern; no Rust type (home.energy is unbound draft).
- **Connection**: NEW `sense-energy` TokenPaste (email:password); authenticates via `POST .../authenticate` and stores session tokens (NOT the password).
- **CSV importer**: Scaffolded; real parser parked. `parser_parked_needs_sample=true` — exact column names undocumented, no sample on disk.
- **Flags**: Needs-login (real Sense account to validate API), Needs-sample (CSV export columns).
- **Key confirmed**: `API_BASE = https://api.sense.com/apiservice/api/v1`; auth response fields `access_token`/`user_id`/`refresh_token`/`monitors[0].id`; usage response `consumption.usage_total_kwh` + `device_breakdown[].consumption.usage_total_kwh`.
- 9 unit tests pass; `cargo check` clean (no new warnings from this module).

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Sense Energy
Monitor (L1840–L1846); at-a-glance L1742. Feasibility 🟡 medium — entirely
the unofficial-API risk; the data itself (appliance-level detection) is
called out as unique. Cross-cutting note 4 sketches the unified energy
schema this will share with Emporia / Enphase / Green Button / Kasa —
sequence those together when the home contract is drafted. Sense is
cloud-only; no local path exists.
