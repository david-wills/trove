# Apple Significant Locations

- **id:** `apple-significant-locations`
- **domains:** `location/` (contract: **Phase 3 pending**). Hypothetical only —
  nothing is written; this entry is an unavailable card.
- **status:** 🚫 unavailable
- **unavailable_reason:** Apple encrypts Significant Locations with keys locked
  in the Secure Enclave — Full Disk Access can open the file but the contents
  are unreadable by anyone, including Apple. No export exists.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** Apple privacy white paper: routined `cache_encryptedA/B.db`
  use Secure-Enclave device keys
- **effort / priority:** XL / P2
- **needs:** privacy (frequent-places trail — moot while inaccessible)

## What it is

macOS/iOS "Significant Locations" (System Settings → Privacy & Security →
Location Services → System Services): the OS's private log of frequently
visited places, used to power predictive features. A rich frequent-places
history in principle, but locked behind device-key encryption with no export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Frequent places | — | none extractable | Secure-Enclave-encrypted DB |

## Access & auth

The `routined` daemon stores data at
`/private/var/folders/.../com.apple.routined/` as `cache_encryptedA.db` /
`cache_encryptedB.db` (iOS: `/private/var/mobile/Library/Caches/com.apple.routined/`).
Full Disk Access can *open* the files, but their contents are AES-encrypted
with keys in the Secure Enclave — forensic tooling (ElcomSoft) cannot read
them without a paired, unlocked device. `locationd`'s `/var/db/locationd/`
holds a readable `clients.plist` plus opaque encrypted blobs. A genuine dead
end, not a permission issue. No standalone-rule concern — there's nothing to
read.

## Vault mapping

- **Raw layer:** none — nothing is written.
- **Contract layer:** none. (Were it accessible, frequent places would land in
  `location/` under the Phase 3 visits shape.)

## Build plan

None to build for data. The shipped `significant-locations` def already
renders this as the honest greyed unavailable card via `Behavior::Unavailable`
(this `apple-significant-locations` brief is the catalog record for that same
provider). Not toggleable, never default-on.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| — | 🚫 | n/a — confirm the card renders greyed with the reason string; the existing `significant-locations` def already does this |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Apple Significant
Locations (macOS) (L2380–L2386). The Mac-Locations-Scraper project targets
iOS *backups* via forensic extraction, not live macOS access — not applicable.
A shipped def (`significant-locations`) already surfaces this as the honest
unavailable card. Photos geotags are the location-history proxy for iPhone
users; Google Timeline import covers the same semantic for Android users.
