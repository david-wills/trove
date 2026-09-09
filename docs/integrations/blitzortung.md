# Blitzortung Lightning

- **id:** `blitzortung`
- **domains:** `environment/` (Unavailable — no folder written)
- **status:** 🚫 unavailable
- **unavailable_reason:** Blitzortung's usage policy forbids direct
  client-to-broker connections — every app install would hit their community
  MQTT broker directly, which a local-first app where each instance connects
  on its own can't comply with. NWS severe-thunderstorm alerts cover the gap.
- **behavior:** Unavailable (not toggleable, never default_on)
- **connection:** none
- **evidence:** official — Blitzortung usage policy; data.blitzortung.org
  requires third-party apps to self-proxy (serve from their own servers
  rather than connect clients directly).
- **effort / priority:** L / P2
- **needs:** none

## What it is

Community volunteer network of ~3,000 lightning-detection sensors globally,
producing excellent open lightning-strike data. The data itself is open and
high quality; the **distribution policy** is the blocker — it's designed to
prevent individual clients hammering the central broker, so third-party apps
must proxy through their own servers. A standalone local-first app where
every install connects directly is exactly the pattern the policy forbids.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lightning strikes | n/a — policy-blocked | strike time, lat/lon, detector positions | data.blitzortung.org (MQTT topics / last_strikes.php) |

Not collectable under the standalone rule — listed for completeness.

## Access & auth

- Data served via MQTT at a public broker (data.blitzortung.org) with
  geohash-based topics; an HTTP endpoint
  (data.blitzortung.org/Data/Protected/last_strikes.php) returns ~100k
  recent strikes. Both require third-party apps to self-proxy per the usage
  policy — **incompatible with the standalone / local-first rule** (no
  central Trove server to proxy through, and direct client connections
  violate the policy).
- Commercial alternatives (OpenWeather Lightning, Xweather, Vaisala) all
  require paid API keys — out of scope for a baked-in keyless collector.

## Vault mapping

- none — Unavailable; no folder written. (Were it collectable it would route
  to `environment/` as a public-feed reading alongside the other ambient
  feeds, per the home-vs-environment owned-device-vs-public-feed rule.)

## Build plan

none — blocked by usage policy. Revisit only on explicit user demand; the
spike would be either Blitzortung MQTT proxying (needs a Trove-operated
relay, which breaks standalone) or evaluating OpenWeather Lightning ($) at
that point.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Lightning strikes | 🚫 | n/a — unavailable; card renders greyed with the reason |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §Blitzortung /
LightningMaps (L2192–L2199). Feasibility 🟠 low. The data is open and
excellent, but the distribution policy (self-proxy requirement) makes direct
integration infeasible for a local-first app. NWS severe-thunderstorm alerts
(keyless, already recommended) cover thunderstorm warnings as a functional
substitute. If lightning-strike proximity becomes a concrete user request,
spike MQTT proxying or evaluate the paid OpenWeather Lightning API then.
