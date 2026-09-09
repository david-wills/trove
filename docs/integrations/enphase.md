# Enphase Solar

- **id:** `enphase`
- **domains:** `home/energy/` (home.energy sibling draft — unbound; raw-only
  rows shaped per home.md energy spec under `home/enphase/energy/`)
- **status:** 🧪 built (2026-06-17)
- **unavailable_reason:** none
- **behavior:** Periodic (15-min LAN poll of the IQ Gateway; Enlighten
  cloud path Needs-David for OAuth app registration)
- **connection:** `enphase` — NEW TokenPaste (`host|token`; 1-year owner
  JWT generated at enlightenapp.com). Not shared with other defs.
- **contract_mode:** deferred-sibling-draft (home.energy) — raw-only until
  home.energy is bound; energy rows already written in the correct shape
- **evidence:** field names confirmed against Matthew1471/Enphase-API
  community docs (Production.adoc: wNow/whToday/whLifetime/readingTime/
  measurementType/rmsCurrent/rmsVoltage/reactPwr/apprntPwr/pwrFactor;
  Inverters.adoc: serialNumber/lastReportDate/devType/lastReportWatts/
  maxReportWatts)

## What it is

Enphase microinverter solar systems, fronted by the IQ Gateway (Envoy) on
the home LAN plus the Enlighten cloud. Solar production and consumption is
high-value personal data with no equivalent source — kWh generated/used,
per-interval, for as long as the system has run. Dual-path: LAN poll for
real-time, cloud for calendar history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Real-time production (local) | none — LAN + owner token | watts now, Wh today/lifetime | community-documented endpoints |
| Per-inverter output (local) | none | per-microinverter watts | `/api/v1/production/inverters` |
| Historical production/consumption (cloud) | Enlighten free **Watt** tier — site-level only | daily/interval site kWh | official v4 docs |
| Microinverter-level history (cloud) | **Kilowatt** tier (paid) | per-device history | official v4 docs |

All optional in the contract; a local-only user simply has no cloud
backfill rows, and a Watt-tier user has site-level granularity. No
tier-specific code paths.

## Access & auth

- **Local:** HTTPS to the IQ Gateway's LAN IP (token-based since firmware
  7.0.x; 1-year token for the system owner). Endpoints:
  `/api/v1/production`, `/api/v1/production/inverters`, etc. Gateway
  serves a **self-signed TLS cert** — pin/accept it deliberately.
- **Cloud:** Enlighten API v4, OAuth, free Watt tier for a personal
  system.
- No TCC. Standalone-clean (LAN HTTPS or plain outbound HTTPS). Local
  path has no rate limits.

## Vault mapping

- **Raw layer:** `home/enphase/YYYY-MM.jsonl` — production/consumption
  readings as pulled (local snapshots + cloud interval rows), full
  fidelity, monthly partitions.
- **Contract layer:** pending Phase 3 home contract — expected one row per
  reading/interval (`ts`, `source`, `guid`, site/device id, watts/Wh
  fields), overflow in `extra`.
- **Dedupe:** `guid` = site or inverter id + interval start; cloud
  backfill cursor in `.trove/`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/enphase.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste for the gateway token with help copy on
   generating it at enphase.com + the 1-year expiry; OAuth method for
   Enlighten), pull hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Token-expiry handling: surface "token expired, re-paste" on the connect
   card (1-year lifetime) — never fail silently.
4. Self-signed-TLS handling for the gateway client (accept the gateway
   cert explicitly, not blanket `accept_invalid_certs` for all hosts).
5. Fixtures from documented/community response shapes (local production,
   inverters, cloud intervals); parser + store + cursor tests.
6. Contract rows wait on the home contract; raw layer can land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local real-time pull | Needs-login | on a LAN with an IQ Gateway, paste address\|token; Sync now; rows in `home/enphase/energy/` + hub last-data |
| Per-inverter rows | Needs-login | verify `circuit` field = serial number in `home/enphase/energy/` |
| Token-expiry UX | Needs-login | paste an expired token; surface "401 / token expired" error in hub |
| Cloud history backfill | Needs-David (OAuth app) | not wired; local path sufficient for real-time |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Enphase Solar
(L1784–L1790). Feasibility 🟢 high. No official public docs for the
*local* endpoint list (community-documented, stable) — keep the cloud
path as the docs-backed reference. Local token refresh is the main
ongoing-maintenance wrinkle. Sibling energy sources (Tesla Powerwall,
Emporia, Sense, Green Button) converge on the same Phase 3 home/energy
shape — sequence one of them nearby to exercise the contract.
