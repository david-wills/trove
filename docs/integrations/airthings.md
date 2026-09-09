# Airthings

- **id:** `airthings`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings
  shape drafted from Hue + Tempest + IAQ sensors + energy monitors together)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (cloud poll for latest samples + historical
  backfill) + Import (dashboard CSV fallback)
- **connection:** `airthings` — TokenPaste (Client ID + Secret from the free
  Airthings developer portal; OAuth client-credentials grant, no browser
  dance). Not shared with other defs.
- **evidence:** official-docs — consumer API documented at
  consumer-api-doc.airthings.com (free registration, latest-samples +
  historical endpoints); dashboard CSV export officially available.
  Consumer API confirmed active as of 2025 (Universal Devices forum, per
  research doc).
- **effort / priority:** S / P2
- **needs:** none

## What it is

Airthings (Wave / Wave Plus / View) is an indoor air quality monitor line
whose standout is **radon** — continuous Bq/m³ measurement that no other
consumer source in the catalog provides — alongside CO2, VOC, PM,
temperature, humidity, and pressure. Cloud-tied (sensors sync via app/hub
to the Airthings cloud), but the consumer API is official and free.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Latest samples | free developer account | radon, CO2, VOC, PM, temp, humidity, pressure per device | official consumer API |
| Historical data | free developer account | same fields over a date range (retention unclear; >1yr observed) | official consumer API |
| Dashboard CSV export | none (account login) | same fields, per device | official dashboard feature |

All optional in the contract; devices lacking a sensor (e.g. no PM on some
Waves) just omit those fields — omit-if-empty, no tier code paths.

## Access & auth

- Consumer cloud API (docs at consumer-api-doc.airthings.com). Auth: OAuth
  **client-credentials** with Client ID + Secret the user creates at the
  Airthings developer portal (free) — machine-to-machine, so it fits
  TokenPaste (paste two values) rather than a browser OAuth dance.
- Endpoints: latest samples per device + historical per device/date-range.
- No TCC, no local files; plain outbound HTTPS. Standalone-clean. The
  separate Airthings *business* API is out of scope.

## Vault mapping

- **Raw layer:** `home/airthings/YYYY-MM.jsonl` — one timestamped row per
  sample, `device` field (serial/name) for multi-sensor homes, full native
  fields.
- **Contract layer:** home contract is **Phase 3 pending**; raw-only until
  ratified (vault-wide conventions apply). Expected readings shape: `ts`,
  `source`, `device`, metric fields (radon included), overflow in `extra`.
- **Dedupe:** `guid` = device serial + sample timestamp; cloud history,
  periodic polls, and CSV imports converge on the same rows. Cursor in
  `.trove/airthings-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/airthings.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: Client ID + Secret fields, setup copy walking
   through the free developer-portal app creation — copy on the def, with
   the disabled-state affordance rule for the connect card), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. On connect: enumerate devices, backfill history (retention is unclear —
   backfill promptly and rely on Trove as the durable record), then poll
   periodically for new samples.
4. CSV import fallback via the generic import box: officially produced
   export, but exact columns unverified — build the importer **parser-last**
   against a real sample if it's prioritized; the API path doesn't need it.
5. Fixtures from the official API docs' example responses (multi-sensor and
   sparse-sensor variants — bindings-optional-fields rule); parser + store +
   cursor tests, unique temp dirs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Latest + history | — | create a free dev-portal client, paste ID+Secret in the connect card, Sync now; confirm rows in `home/airthings/` + hub last-data; radon field present on a Wave Plus |
| CSV import | — | export a device CSV from the dashboard, drop in import box, confirm merge without duplicate guids |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Airthings Wave /
View (L1832–L1838); at-a-glance L1741; also referenced from the Awair entry
(L1830). Feasibility 🟢 high. Radon is the unique draw. Cloud-only (no local
API path, unlike Awair) — cross-cutting note 1's tier (b): ongoing token
management. Historical retention undocumented (>1 year observed) — treat
backfill-on-connect as load-bearing. Pair with Awair for full IAQ coverage;
both feed the same pending home readings contract.
