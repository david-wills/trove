# AwardWallet

- **id:** `awardwallet`
- **domains:** `travel/` (trip-segment shape — **Phase 3 pending**) ·
  `finance/purchases/` (loyalty balance snapshots / points line items —
  **Phase 3 pending** purchase line-item contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the Account / Email-Parsing API; monthly
  balance snapshots + new reservations; watermark cursor)
- **connection:** `awardwallet` — TokenPaste (API key, NOT OAuth — the brief
  was incorrect; the live Account Access API uses `X-Authentication: <api_key>`
  header auth; a Business account is required). Full travel-timeline export
  requires a paid AwardWallet Business subscription.
- **evidence:** official-docs — awardwallet.com/api/account (Account Access,
  OAuth), awardwallet.com/api/main (Email Parsing); 700+ loyalty programs.
  Paid-tier caveat for the travel timeline.
- **effort / priority:** M / P2
- **needs:** privacy (loyalty balances = financial detail; the Web Parsing
  API stores loyalty credentials — opt-in with explicit acknowledgement,
  and we prefer the credential-free paths) · Needs-login (validation needs a
  real account; build proceeds from documented shapes)

## What it is

Loyalty-program aggregator: tracks miles and hotel points across 700+
programs (Marriott Bonvoy, Hilton Honors, IHG, Hyatt, American AAdvantage,
United MileagePlus, Delta SkyMiles, Southwest, Alaska, …) plus a parsed
travel reservation timeline. Useful for frequent travelers who want point
balances and trip records collected over time — the structural flight data
from Flighty/TripIt/flight-email parsing is what these balances relate to,
so AwardWallet sequences after flight import is stable.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Loyalty balances | Account Access API (OAuth) | program, balance, member id | official docs |
| Travel reservations | paid Business sub for full timeline | flights, hotels, cars — dates, confirmation, segments | official docs |
| Email-parsed reservations | Email Parsing API (most accessible) | reservation extracted from confirmation email body | official docs |

All optional in the contracts; a free user's run yields balances + whatever
the Email Parsing path extracts, no full timeline.

## Access & auth

- Three APIs: (1) **Account Access** — OAuth, reads balances + reservations
  for AwardWallet users (awardwallet.com/api/account); (2) **Web Parsing** —
  caller supplies stored loyalty credentials, returns balance + history
  (privacy-sensitive, opt-in only); (3) **Email Parsing** — extracts
  reservation data from confirmation email bodies (awardwallet.com/api/main).
- The **Email Parsing API is the most accessible and privacy-compatible
  path** for Trove; the Web Parsing API (stores loyalty credentials) is
  gated behind explicit opt-in if offered at all.
- Paid Business subscription required for the full travel-timeline export.
- Standalone-clean: plain HTTPS, no TCC.

## Vault mapping

- **Raw layer:** `travel/awardwallet/raw/YYYY-MM.jsonl` (reservation +
  account objects) and `finance/purchases/awardwallet/raw/YYYY-MM.jsonl`
  (balance snapshots) — full fidelity.
- **Contract layer (records route by shape):**
  - Trip segments (flights/hotels/cars) → `travel/` per the **pending
    Phase-3 trip-segment contract** (one row per segment: `ts`, `guid`,
    segment type, start/end, confirmation, origin/destination).
  - Loyalty **balance snapshots** → `finance/purchases/awardwallet/…` per
    the **pending Phase-3 purchase/line-item contract** (monthly snapshot of
    program → balance; earn/burn derived at read time).
- Both contracts unratified → provider **parked behind Needs-David
  (contract)** until they land.
- **Dedupe:** reservation id as `guid` for segments; (program, snapshot
  month) key for balances. Cursor in `.trove/`, rebuildable from output.

## Build plan

1. Module `crates/trove-core/src/awardwallet.rs`: `DEF` (Periodic),
   `CONNECTION` (OAuth — Account Access; label/help per affordance rule),
   `pull` hook. Default to the Email Parsing path; gate Web Parsing
   (credential storage) behind an explicit opt-in acknowledgement.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; multi-domain
   (`travel`, `finance/purchases`) — split writes by record shape, never one
   record across folders.
3. Fixtures from documented Account / Email-Parsing responses (balance-only
   AND reservation-bearing); parser + store + cursor tests, unique temp dirs.
4. Privacy gate: ships opt-in (financial detail — point balances). On a
   paid-tier 403 for the full timeline, degrade to balances + email-parsed
   reservations with a UI hint, never fail.
5. Vault writes via `store` helpers once the travel and purchase contracts
   are ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Balances | 🧪 built | paste API key; Sync now; confirm balance snapshots in `travel/awardwallet/raw/balances/` + hub last-data (developer/Business account) |
| Travel segments | 🧪 built (paid tier) | requires a paid Business sub for `/travel-timeline/`; paste key; Sync now; confirm `travel/awardwallet/YYYY-MM.jsonl` has flight/hotel/car Segment rows |
| 403 degradation | 🧪 built | on a free-tier key, the timeline endpoint returns 403; balance snapshot still succeeds; no error surfaced to the UI |

## Build notes (2026-06-17)

- **Auth correction:** the brief described "OAuth" but the live API uses static
  API key authentication (`X-Authentication` header). Fixed in this build;
  connection changed from `ConnectMethod::OAuth` to `ConnectMethod::TokenPaste`.
- **Balance snapshots:** raw-only (`travel/awardwallet/raw/balances/YYYY-MM.jsonl`).
  These are point-balance snapshots (not transactions/purchases), so they do NOT
  fit `finance-purchases.LineItem`; they fit the unbound `finance-holdings` draft.
  `contract_mode = deferred-sibling-draft` for the balance slice;
  `contract_mode = reuse-bound` (travel.Segment) for reservations.
- **Travel segments:** flight, hotel, and car reservations all map to
  `travel.Segment` on the bound travel contract (same contract as Airbnb/Flighty).
- **14 tests, all green.** Offline trait injection covers flight/hotel/car mapping,
  idempotency, 403 degradation, cursor persistence, and balance raw writes.
- **Needs-login:** a Business account + API key is required; build proceeds from
  the documented field shapes (Swagger UI confirmed). Parser is NOT parked —
  field names are from official published docs, not community-folklore.

## Research notes

`integrations-research.md` → "Geolocation & Travel" §AwardWallet (Loyalty
Programs) (L2420–L2426). Feasibility 🟡 medium — API is live and
commercial but the full travel timeline needs a paid Business account; the
Email Parsing API is the accessible entry point. Web Parsing stores loyalty
credentials — keep it opt-in only and prefer credential-free paths.
Sequence after flight record import (Flighty/TripIt/flight-emails) is stable,
since flights provide the structural data balances relate to.
