# Overland (iOS GPS Logger)

- **id:** `overland`
- **domains:** `location/` (contract: **Phase 3 pending** — location; trails
  shape now)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Live (troved exposes a local HTTP receiver; the Overland app
  POSTs batches to it)
- **connection:** none — the user points the Overland iPhone app at a local
  `troved` endpoint; no login. Authentication is the local-network reachability
  + an optional shared token, not an OAuth dance.
- **evidence:** official open source — github.com/aaronpk/Overland-iOS (Apache
  2.0); GeoJSON FeatureCollection POST payload documented
- **effort / priority:** M / P2
- **needs:** **privacy** (continuous location trail — mandatory opt-in with
  explicit acknowledgement); requires a reachable `troved` receiver endpoint

## What it is

Overland is the canonical open-source always-on iOS GPS logger (IndieWeb
favorite). It batches location samples offline and POSTs them as GeoJSON to a
user-configured endpoint. Trove's role is the **receiver**: `troved` exposes a
local HTTP endpoint the phone posts to. Power-user opt-in — the most complete
continuous-trail source we can capture without a cloud dependency.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Location batches | free / open source | coordinates, timestamp, speed, altitude, horizontal accuracy | official repo |
| Device context | free | battery level, motion type, wifi SSID | official repo |

All optional in the location contract; omit-if-empty for fields a given sample
lacks.

## Access & auth

- Mechanism: **troved local HTTP receiver** (e.g. `/overland`). The Overland app
  is configured to POST GeoJSON FeatureCollection batches to that endpoint;
  reachability is "same WiFi or tunneled." The app buffers offline and
  batch-sends.
- Auth: no OAuth — an optional shared secret/token in the URL guards the
  endpoint. No cloud, no external service: **standalone-clean by construction**
  (the data never leaves the user's network).
- Overland can emit **either** native Overland format **or** OwnTracks format —
  share **one receiver with OwnTracks** and detect the format per request.

## Vault mapping

- **Raw layer:** `location/overland/raw/YYYY-MM.jsonl` — the posted GeoJSON
  features, full fidelity.
- **Contract layer:** `location/overland/YYYY-MM.jsonl` per the **pending
  Phase-3 location contract** (trails shape): one row per fix (`ts`, `source`,
  `guid`, `lat`, `lon`, `altitude`, `speed`, `accuracy`), device context
  (battery, motion, SSID) in `extra`. Trails shape exists now; visit-shaped
  records wait for a visits-shaped source.
- **Dedupe `guid`:** per-fix identity = (timestamp + lat + lon) hash; batches
  may overlap on re-send, so dedupe on ingest. Receiver is append-only; no
  cursor needed (the phone owns send-state).

## Build plan

1. **troved receiver first:** a minimal HTTP endpoint in `troved` that accepts
   POSTed GeoJSON, validates an optional token, and hands batches to
   `trove-core`. **Shared with OwnTracks** — branch on payload shape
   (FeatureCollection vs `_type:location`).
2. Module `crates/trove-core/src/overland.rs`: `DEF` (Live), parser for the
   GeoJSON FeatureCollection payload.
3. **Privacy gate:** ships opt-in (continuous location trail) — explicit
   acknowledgement on enable; never default_on.
4. Fixtures from the documented Overland payload (single + multi-feature
   batches, offline-buffered backfill); parser + store + dedupe tests, unique
   temp dirs.
5. Vault writes via `store` helpers once the location contract is ratified;
   until then **parked behind the pending location contract**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Location batches | — | install Overland on an iPhone, point it at the local `troved` endpoint, walk a route; confirm fixes land in `location/overland/` + hub last-data |
| Offline backfill | — | put the phone in airplane mode mid-route, re-enable network; confirm buffered batch arrives and dedupes cleanly |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Overland (L2300–L2306).
Feasibility 🟡 medium — best open-source always-on GPS path, but requires iPhone
app setup + network reachability, so it ranks below importing existing location
data. GeoJSON payload carries coords, speed, altitude, battery, wifi SSID,
motion type. **OwnTracks is the same M2 mechanism** — build one shared receiver
with format detection.
