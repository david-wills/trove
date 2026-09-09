# Apple Maps Visited Places

- **id:** `apple-maps`
- **domains:** `location/` (contract: **Phase 3 pending** — trails shape now;
  visits await a visits-shaped source). Hypothetical only — nothing is
  written; this entry is an unavailable card.
- **status:** 🚫 unavailable
- **unavailable_reason:** Apple end-to-end encrypts Visited Places on the
  iPhone with device keys — no app, including Trove, can read it, and Apple
  offers no export. The feature doesn't exist on the Mac.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** Apple privacy docs + MacRumors/9to5Mac (Mar 2026): on-device,
  E2E encrypted, no export
- **effort / priority:** XL / P2
- **needs:** privacy (location trail — would ship opt-in *if* it were ever
  accessible; currently moot)

## What it is

iOS 26's opt-in "Visited Places" log in the Maps app (Profile → Places →
Visited Places): a private, on-device record of places the user has been.
Semantically high-value as a personal location history, but built from the
ground up to be unreadable by anyone but the device owner inside the Maps UI.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Visited places | — | none extractable | E2E-encrypted on-device |

## Access & auth

iOS-26-only feature. Data is stored on the iPhone (likely under
`/private/var/mobile/Library/Caches/com.apple.Maps/`), end-to-end encrypted
with device-specific keys, never synced to iCloud in readable form, and never
present on the Mac. No export, no API, no entitlement a third-party app can
hold. Reading it would require a companion iOS app inside Apple's
CoreLocation/HealthKit framework — out of scope and still blocked by the
encryption.

## Vault mapping

- **Raw layer:** none — nothing is written.
- **Contract layer:** none. (Were it accessible, place visits would land in
  `location/` once a visits-shaped contract is drafted in Phase 3.)

## Build plan

None. Renders as the honest greyed unavailable card driven by
`Behavior::Unavailable`. Not toggleable, never default-on.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| — | 🚫 | n/a — no data path exists |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Apple Maps Visited Places
(iOS 26) (L2372–L2378). Hard encryption wall, identical to Significant
Locations. Monitor for a future official export (possible GDPR pressure).
Photos geotags are the closest location-history proxy on the Mac.
