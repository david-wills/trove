# Transit Cards (Clipper, Oyster, ORCA, …)

- **id:** `transit-cards`
- **domains:** `travel/` (out-of-scope catalog entry; no folder is written —
  vault_path "")
- **status:** 🚫 unavailable
- **unavailable_reason:** Transit card operators (Clipper, Oyster, ORCA,
  Ventra, …) offer no API and at best PDF-only statements — there is no
  reliable machine-readable way to get your journey history.
- **behavior:** Unavailable (never toggleable, never default_on)
- **connection:** none
- **evidence:** community — Clipper/ORCA/Oyster web UIs are PDF-only or have
  no export; the 2019 Clipper API proposal is dead
- **effort / priority:** L / P2
- **needs:** privacy (journey/location trail — would ship opt-in if ever
  buildable)

## What it is

Stored-value transit fare cards (Clipper in the Bay Area, Oyster in London,
ORCA in Seattle, Ventra in Chicago, CharlieCard in Boston, …). Their tap
history is a fine-grained commute/location trail — high-value in principle,
but unobtainable in a machine-readable form today.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Journey/tap history | n/a | (PDF-only or unavailable) | community — no export |

Nothing machine-readable is offered to riders, so there are no contract
fields to map.

## Access & auth

- No public APIs for any major operator. Clipper (clippercard.com) shows
  transaction history but exports **PDF only**. Oyster (oyster.tfl.gov.uk)
  shows **8 weeks** of journey history as a web view only. ORCA, Ventra,
  CharlieCard — similar: web UI, no structured export.
- Each operator would need a separate, fragile PDF parser, with login behind a
  manual web download. Not a reliable standalone path.

## Vault mapping

- **Raw layer:** none (nothing to collect).
- **Contract layer:** none. If a future operator ships CSV/JSON, the
  trip/journey rows would route to `travel/transit-cards/` under the Phase 3
  travel contract; privacy-sensitive (location trail) → opt-in.

## Build plan

None. Catalogued as unavailable so the hub answers "why isn't my transit card
here?" honestly. Revisit if any major operator introduces a CSV/JSON export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| (unavailable) | 🚫 | n/a — card renders dim with the unavailable reason |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Transit Card History
(L2436–L2442). Feasibility 🟠 low. Clipper moved to Clipper 2.0 (Dec 2025,
tap-to-pay) but still no rider API; Oyster keeps only 8 weeks. The pragmatic
substitute already exists: the bank/card CSV import captures transit **fare
charges** (spending), and Apple/Google Maps trip history covers commute
patterns — so the journey-history gap is partially backfilled elsewhere.
Privacy: were this ever buildable, the journey trail is a location trail and
would ship opt-in with explicit acknowledgement.
