# OwnTracks

- **id:** `owntracks`
- **domains:** `location/` (contract: **Phase 3 pending** — location; trails
  shape now)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Live (troved exposes a local HTTP receiver; the OwnTracks app in
  HTTP mode POSTs JSON payloads to it)
- **connection:** none — the user configures the OwnTracks app's HTTP mode to a
  local endpoint; no login. Optional shared token guards the endpoint.
- **evidence:** official open source — owntracks docs + Recorder reference impl
  (github.com/owntracks/recorder); JSON `_type:location` payload documented
- **effort / priority:** M / P2
- **needs:** **privacy** (continuous location trail — mandatory opt-in with
  explicit acknowledgement); requires a reachable `troved` receiver endpoint

## What it is

OwnTracks is an open-source iOS/Android GPS logger, the other half of the
self-hosted-location pair with Overland. In **HTTP mode** it POSTs JSON
location payloads to a user-configured endpoint (we skip MQTT mode — no broker
in a local-first app). iOS 16+, actively maintained (updated for iOS 18 in
2025). Power-user opt-in.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Location reports | free / open source | lat, lon, tst (Unix ts), acc, vel, alt, batt, topic | official docs |
| Region events | free | enter/leave waypoint (geofence) events | official docs |

All optional in the location contract; region events are a distinct shape from
location fixes — keep them in `extra` or a sidecar, don't force them into the
trails row.

## Access & auth

- Mechanism: **troved local HTTP receiver** (HTTP mode only — **skip MQTT**, it
  needs a broker the standalone rule forbids). OwnTracks' Significant Location
  Change mode reduces battery drain.
- Auth: no OAuth — optional shared secret/token on the endpoint. No cloud, no
  external service: **standalone-clean** (data stays on the user's network).
- Payload is JSON with `_type:location` (vs Overland's GeoJSON
  FeatureCollection) — **share one receiver with Overland** and detect format
  per request.

## Vault mapping

- **Raw layer:** `location/owntracks/raw/YYYY-MM.jsonl` — the posted JSON
  payloads, full fidelity.
- **Contract layer:** `location/owntracks/YYYY-MM.jsonl` per the **pending
  Phase-3 location contract** (trails shape): one row per fix (`ts` = `tst`,
  `source`, `guid`, `lat`, `lon`, `alt`, `vel`, `acc`), battery and topic in
  `extra`. Region enter/leave events are a separate type — they don't split a
  record, they're their own rows (or `extra`-tagged), routed whole.
- **Dedupe `guid`:** (tst + lat + lon) hash; receiver is append-only and
  dedupes on ingest since batches can overlap. No cursor (the phone owns
  send-state).

## Build plan

1. **Shared troved receiver** (the same endpoint Overland uses) — branch on
   payload shape: `_type:location` JSON → OwnTracks parser; GeoJSON
   FeatureCollection → Overland parser. Build the receiver once.
2. Module `crates/trove-core/src/owntracks.rs`: `DEF` (Live), parser for the
   `_type:location` JSON payload (and `_type:transition` region events).
3. **Privacy gate:** ships opt-in (continuous location trail) — explicit
   acknowledgement on enable; never default_on.
4. Fixtures from the documented OwnTracks payloads (location + transition);
   parser + store + dedupe tests, unique temp dirs.
5. Vault writes via `store` helpers once the location contract is ratified;
   until then **parked behind the pending location contract**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Location reports | — | configure OwnTracks HTTP mode at the local `troved` endpoint, move around; confirm fixes in `location/owntracks/` + hub last-data |
| Region events | — | define a waypoint, enter/leave it; confirm transition events arrive and route as their own rows, not merged into a fix |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §OwnTracks (L2308–L2314).
Feasibility 🟡 medium — same class as Overland, requires user setup; HTTP mode
is simplest. The OwnTracks Recorder (C) is the reference server; Trove
implements a minimal receiver in `troved`. **Build alongside Overland with one
shared endpoint and format detection** — they're the same M2 mechanism.

## Build notes (2026-06-16)

Built as `Behavior::Live` with a channel-based `LiveCollector`. The `ingest(vault, body)`
function is the public entry point for troved's HTTP handler (not yet wired in troved —
the HTTP endpoint and shared-receiver-with-Overland are the remaining troved pieces).

- **Parser:** `_type:location` and `_type:transition` fully parsed from the official
  OwnTracks JSON booklet. All other types (lwt, waypoint, card, etc.) stored raw-only.
- **Raw:** `location/owntracks/raw/YYYY-MM.jsonl` — every POST verbatim, month-partitioned.
- **Contract:** `location/owntracks/YYYY-MM-DD.jsonl` — one `Fix` per location fix
  (and per transition with lat/lon), day-partitioned. Deduplicated by
  `guid = ot-{tst}-{lat6}-{lon6}`.
- **vel conversion:** OwnTracks sends km/h; `Fix.speed` is m/s — converted at parse time.
- **zero-val suppression:** `vel=0` and `acc=0` are "unknown" in OwnTracks spec — omitted
  from `Fix.speed`/`Fix.accuracy`.
- **Transition → Fix:** only when lat/lon are present; `extra.ot_event=enter|leave`,
  `extra.ot_region`, `extra.ot_rid` carry the geofence context.
- **Global channel:** `GLOBAL_TX` + `push_body()` are the wiring seam troved uses to
  push HTTP POST bodies into the live collector tick loop.
- 19 unit tests, all green (`cargo test -p trove-core owntracks::`, `cargo check`).
