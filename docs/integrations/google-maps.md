# Google Maps Saved Places

- **id:** `google-maps`
- **domains:** `location/` (contract: **Phase 3 pending** — places-of-interest
  / visits layer; the trails shape exists now, a visits shape waits for a
  visits-shaped source)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Google Takeout download; no API)
- **connection:** none — the user downloads Saved Places via Google Takeout
  (login on Google's site). No stored credential.
- **evidence:** official-docs — Takeout → Maps (your places) exports a GeoJSON
  `FeatureCollection`; easy parse (high confidence)
- **effort / priority:** S / P2
- **needs:** privacy (location data — opt-in with explicit acknowledgement)

## What it is

A user's **curated** Google Maps places — Starred, Labeled (Home/Work), Want
to go, and other saved lists. This is *not* visit history (Timeline moved
on-device and is out of Takeout); it is the user's places-of-interest layer:
favorites and saved locations, useful as a context layer joined against actual
location trails at read time.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved places | all accounts | place name, address, coordinates, optional URL | official Takeout (GeoJSON) |
| List membership | all accounts | which list (Starred / Labeled / Want to go) | official Takeout |

All optional in the contract (omit-if-empty).

## Access & auth

- Google Takeout (`takeout.google.com`) → **Maps (your places)** → download.
  Format: GeoJSON `FeatureCollection`, one file per saved list. No API, no
  token, no rate limit.
- No TCC. Standalone-clean (parse a local GeoJSON file). Bundles with the
  Timeline / Takeout import infrastructure.

## Vault mapping

- **Raw layer:** `location/google-maps/raw/` — the GeoJSON features verbatim,
  full fidelity.
- **Contract layer:** `location/…` per the (pending) location places shape —
  expected one row per saved place (`source`, `guid` = place id/coords,
  `name`, `lat`/`lon`, `address`, `list`), no `ts` (these are not events),
  overflow (URL, label) in `extra`. **Records route whole**: this is the
  places-of-interest layer, distinct from GPS trails. Parked behind the
  contract draft (Needs-David).
- **Dedupe:** stable place id (or name+coords) as `guid`; re-import replaces.

## Build plan

1. Module `crates/trove-core/src/google-maps.rs`: `DEF` (Import), `last_data`
   hook; no `CONNECTION`.
2. Registration line in `INTEGRATIONS`.
3. Wire to the generic Takeout/ZIP import box — detect the Maps (your places)
   GeoJSON subtree, parse the `FeatureCollection`, tag each feature with its
   list.
4. **Privacy gate:** ships opt-in (location data) — explicit acknowledgement
   on enable, even though these are curated places rather than a trail.
5. Fixtures: a sample Saved Places GeoJSON (Starred + Labeled Home/Work +
   Want to go); parser + store tests, unique temp dirs.
6. Vault writes via `store` helpers once the location contract is ratified.

## Build notes (2026-06-16)

- **Contract mode: raw-only.** `location.rs` explicitly excludes saved places from the `Fix`
  contract ("stays raw until a visits-shaped contract lands"); no Fix rows written.
- **Format detection:** top-level `"type": "FeatureCollection"` + first feature has `properties.Title`.
  Empty FeatureCollections accepted (valid export of an empty list).
- **Zip support:** all `.json` entries under a `Maps`-named folder are scanned; the archive-browser
  and other Takeout decoys are skipped.
- **Idempotent:** content-hash naming (`<slug>-<fnv1a64>.json`) deduplicates re-drops of the same file.
- **GeoJSON fields preserved verbatim:** `properties.Title`, `Published`, `Updated`, `Google Maps URL`, `Note`,
  `Location.Address`, `Location.Country Code`, `geometry.coordinates [lon, lat]`.
  (Real Takeout exports use `"Google Maps URL"` as the property key, not `"URL"`.)
- 15 tests, all green; cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Saved places | ✅ built (raw) | export a real Maps (your places) Takeout, drop into the import box; confirm files in `location/google-maps/raw/` + hub last-data; spot-check Home/Work labels |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Google Maps Visited
Places (L2444–L2450). Feasibility 🟢 high. **Saved Places ≠ Timeline** — this
is user-curated favorites, not visit history (Timeline moved on-device and is
not in Takeout). Lower priority than raw GPS trails but a useful
places-of-interest layer. Bundle with the Timeline importer's infrastructure.
