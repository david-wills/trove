# Google Pollen

- **id:** `google-pollen`
- **domains:** `environment/` (contract: **Phase 3 pending** — ambient/public
  feeds; existing `weather/` stays where it is, catalogued under this domain)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the forecast once daily for the user's
  location)
- **connection:** `google-pollen` — TokenPaste (a Google **Cloud API key**,
  *not* the existing `google` OAuth connection — different auth model: an API
  key tied to a billing-enabled Cloud project). Not shared with other defs.
- **evidence:** official-docs — `pollen.googleapis.com/v1/forecast:lookup`,
  documented, 65+ countries (high confidence)
- **effort / priority:** M / P2
- **needs:** Needs-login (Cloud API key + billing account setup — validation
  needs a real key; build proceeds from documented shapes)

## What it is

Google's pollen-forecast API: daily tree / grass / weed forecasts with
species-level breakdown and a UPI (Universal Pollen Index), covering 65+
countries with the best US coverage of the available feeds. A public ambient
feed (not an owned device) → `environment/`. For Europe, keyless Open-Meteo
pollen suffices; Google Pollen earns its keep for US and global reach.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Daily pollen forecast | free tier 5k calls/mo (billing acct required) | per-day tree/grass/weed UPI index | official docs |
| Species breakdown | same | named species (oak, birch, ragweed, …) with index | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- REST: `GET pollen.googleapis.com/v1/forecast:lookup?location.latitude=LAT
  &location.longitude=LON&days=5&key=API_KEY`. Returns daily forecast for the
  given coordinates.
- **Auth = Cloud API key**, not OAuth. A Google Cloud **billing account is
  required even for the free tier** (5,000 calls/mo per SKU) — this is the
  friction point and the reason for the dedicated TokenPaste connection rather
  than the shared `google` login.
- Rate budget: ~1 call/day = ~365/year, trivially inside the 5k/mo free tier.
- No TCC, no local files; plain HTTPS, standalone-clean.

## Vault mapping

- **Raw layer:** `environment/google-pollen/raw/YYYY-MM.jsonl` — the forecast
  response objects, full fidelity.
- **Contract layer:** `environment/…` per the (pending) ambient-readings
  shape — expected one row per day per location (`ts` = forecast date,
  `source`, `guid` = `date+coords`, reading type = "pollen", index values),
  species breakdown in `extra`. **`home/` vs `environment/`:** this is a
  public feed, so `environment/`; same-shaped readings from an owned sensor
  would merge at read time. Parked behind the contract draft (Needs-David).
- **Dedupe:** `{date}:{lat,lon}` as `guid`; cursor in
  `.trove/google-pollen-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/google-pollen.rs`: `DEF` (Periodic, daily),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule — spell out that an API key *and a billing-enabled Cloud
   project* are required), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Location source: reuse the user's configured location (same as `weather/`)
   rather than asking again.
4. Fixtures from the official example response (multi-day, multi-species);
   parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers once the environment contract is ratified.

## Build notes (2026-06-17)

- NEW `ConnectionDef` (`id = "google-pollen"`) — TokenPaste for the Cloud API key;
  NOT the shared `google` OAuth connection (different auth model: billing-keyed project).
- Reuses `environment::EnvReading` contract (metric = `pollen_grass_upi` /
  `pollen_tree_upi` / `pollen_weed_upi`; unit = `"index"`; value = UPI 0–5).
- Raw layer: full `DayInfo` objects verbatim at `environment/google-pollen/raw/YYYY-MM.jsonl`.
- Contract layer: `environment/google-pollen/YYYY-MM.jsonl` — one reading per
  aggregate pollen type per day; species breakdown (`graminales`, `oak`, `ragweed`, …
  lowercased plant codes → UPI value) in `extra.species`.
- guid = `gpollen:{YYYY-MM-DD}:{metric}:{lat},{lon}` — stable across re-polls; upserts.
- Cursor: `.trove/google-pollen-sync.json` (non-secret; rebuildable).
- 16 tests, all green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Daily forecast | ✅ built | paste a real Cloud API key (billing enabled) in the connect card; Sync now; confirm daily rows in `environment/google-pollen/` + hub last-data |
| Species breakdown | ✅ built | confirm per-species index values land in `extra.species` for a region in season |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §Google Maps
Pollen API (L2184–L2191). Feasibility 🟡 medium — purely because of the Cloud
**billing-account requirement** (even the free tier needs one). Introduced
~2023, 65+ countries, best US coverage. Keyless **Open-Meteo pollen** is the
Europe-sufficient alternative; **Ambee** is a paid alternative. Worth it for
users willing to set up billing for US/global pollen.
