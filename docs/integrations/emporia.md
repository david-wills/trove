# Emporia Vue

- **id:** `emporia`
- **domains:** `home/` (contract: **Phase 3 pending** — research doc sketches
  a shared energy-readings shape across Sense / Emporia / Enphase / Green
  Button / Kasa; the Phase 3 home contract decides it)
- **status:** 🧪 built (Periodic/15min, TokenPaste Cognito SRP, home.energy raw draft)
- **unavailable_reason:** none
- **behavior:** Periodic (unofficial cloud API poll + historical backfill)
- **connection:** `emporia` — TokenPaste (Emporia account credentials;
  **unofficial** API, labeled as such). Not shared with other defs.
- **evidence:** community-schema — pyemvue (pypi.org/project/pyemvue),
  explicitly acknowledged by Emporia as community-supported but unsupported
  by them; used in production (vuegraf). Confidence medium-high for shape,
  medium for longevity. No official API, no CSV export documented — no
  fallback path.
- **effort / priority:** M / P2
- **needs:** Needs-login (real account to validate; no simulator, no export
  to fixture from)

## What it is

Emporia Vue is a panel-mounted home energy monitor that measures usage
**per circuit** via clamp sensors (up to 16 circuits on Vue 2, 18 on the
2026 Vue 3) — actual measured per-circuit kWh rather than Sense's inferred
per-appliance detection. All data flows through Emporia's cloud; there is
no local API on the device and no export feature, so the unofficial cloud
API is the only path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Per-circuit usage | none | kWh per circuit at 1-second to daily resolution | pyemvue community library |
| Whole-home + mains | none | aggregate usage, net metering | pyemvue |
| Solar net metering | none (solar setups) | generation vs consumption | pyemvue ("solar net metering supported") |
| Device/circuit list | none | circuit names, device metadata | pyemvue |

All optional in the contract; non-solar homes simply have no generation
rows — omit-if-empty.

## Access & auth

- Unofficial cloud API (reverse-engineered by pyemvue): authenticate with
  Emporia account credentials, pull per-circuit energy at chosen
  resolutions (1s / 1min / hourly / daily). Emporia has stated a long-term
  official-API goal but no timeline; they may break the unofficial one.
- No local API (the device runs no LAN server) and no CSV export — there is
  **no fallback** if the cloud path breaks; the brief's honest failure copy
  matters more here than usual.
- No TCC, no local files; outbound HTTPS only. Standalone-clean: reimplement
  the documented HTTP calls in Rust (research doc: "ported to Rust possible
  via HTTP client"); never shell out to the Python library.
- Connect-card copy must label the integration community-maintained /
  unofficial.

## Vault mapping

- **Raw layer:** `home/emporia/YYYY-MM.jsonl` — one row per circuit per
  interval (`device`/`circuit` fields, interval resolution recorded);
  sensible default cadence is 1-minute or hourly aggregates, not the
  1-second firehose (collection-depth toggle if finer data is wanted).
- **Contract layer:** home contract is **Phase 3 pending**; raw-only until
  ratified. Feeds the same energy-readings sketch as Sense/Enphase/Green
  Button ({ts, source, direction, watts_or_kwh, interval}); the taxonomy
  routes it under `home/`, not a separate `energy/` folder.
- **Dedupe:** `guid` = circuit id + interval start + resolution; backfills
  and polls converge. Cursor in `.trove/emporia-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/emporia.rs`: `DEF` (Periodic — energy
   cadence per cross-cutting note 2, ~1-minute class), `CONNECTION`
   (TokenPaste credentials, unofficial-API setup copy), `pull` hook with
   historical backfill on first connect.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Resolution strategy: backfill hourly/daily aggregates deep, then poll
   1-minute going forward; expose a depth toggle rather than hardcoding
   (collection-depth-is-configurable rule).
4. Fixtures from pyemvue's documented request/response shapes; parser +
   store + cursor tests, unique temp dirs. Real-account validation is
   Needs-login (David is not known to own one — any Vue owner's run
   validates).
5. Failure posture: on auth/shape break, card shows "Emporia changed their
   unsupported API — sync paused" — never silent, never crash-loop.

## Build notes (2026-06-17)

- **Auth**: AWS Cognito SRP (USER_SRP_AUTH) implemented from scratch in Rust using
  `num-bigint` + existing `sha2`/`hmac` deps.  Pool: `us-east-2_ghlOXVLi1`,
  client: `4qte47jbstod8apnfic0bunmrq`.  Token refresh on expiry.
- **Connection**: New `ConnectionDef` (`id="emporia"`, TokenPaste: `email:password`).
  Added `&crate::emporia::CONNECTION` to CONNECTIONS in integrations.rs.
- **contract_mode**: `deferred-sibling-draft` — `home.energy` is unbound draft.
  Raw energy rows written to `home/emporia/energy/YYYY-MM.jsonl` following the
  home.energy schema field-for-field (`ts`, `source`, `device`, `circuit`, `kwh`,
  `interval_secs`, `direction`, `guid`) for forward compatibility.
- **Raw layer**: `home/emporia/raw/YYYY-MM.jsonl` — device list + per-channel
  chart responses, full fidelity.
- **Cursor**: `.trove/emporia-sync.json`, per-channel watermarks (epoch secs).
  Default backfill window: 30 days.  Scale: `1MIN` (minute-level).
- **Dep added**: `num-bigint = "0.4"` (pure Rust, no C/cmake).
- **Needs-login**: Needs a real Emporia account to validate (confirmed unofficially
  via pyemvue / vuegraf communities; cannot test without hardware).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Per-circuit poll + backfill | Needs-login | paste `email:password`, Sync now; confirm rows in `home/emporia/energy/` + raw in `home/emporia/raw/` |
| Solar net metering | Needs-login | requires a solar-equipped Vue; `direction` field carries `consumption` (consumption-only direction hardcoded — solar/production direction deferred to when we can test it) |
| Token refresh | Needs-login | wait for token expiry or test with an expired token in secret store |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Emporia Vue
(L1848–L1854); at-a-glance L1743. Feasibility 🟡 medium — the data path
works today and is acknowledged by the vendor, but it is unsupported and
cloud-only with no export safety net. Complements Sense (measured circuits
vs inferred appliances) and Kasa plugs (per-plug) in the energy picture;
sequence with the other energy sources when the Phase 3 home contract is
drafted (cross-cutting note 4).
