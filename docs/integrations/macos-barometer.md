# Mac Barometer

- **id:** `macos-barometer`
- **domains:** `environment/` (no folder assigned — out of scope; see below)
- **status:** 🚫 unavailable
- **unavailable_reason:** Apple does not expose the Mac's barometer to apps —
  CMAltimeter is unavailable on macOS regardless of hardware. Pressure is
  already collected from Open-Meteo (`pressure_msl` in the weather stream).
- **behavior:** Unavailable
- **connection:** none
- **evidence:** official-docs — Apple Developer Forums confirm `CMAltimeter`
  is marked unavailable on macOS (the CoreMotion altitude/pressure APIs ship
  in the SDK for Mac Catalyst but are compiled out on the platform)
- **effort / priority:** XL / P2
- **needs:** none

## What it is

Apple-Silicon Macs contain barometer hardware, but macOS provides no public
API to read it. CoreMotion's `CMAltimeter` (which surfaces barometric
pressure on iOS/watchOS) is explicitly unavailable on macOS, so an app cannot
sample the onboard sensor. Catalogued here so the in-app card answers the
"why can't Trove read my Mac's barometer?" question honestly.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Barometric pressure | n/a | none — API blocked on macOS | Apple Developer Forums |

## Access & auth

`CoreMotion.CMAltimeter` on iOS/watchOS exposes `relativeAltitude` and
`pressure`; on macOS the altitude/pressure members are marked unavailable.
No path exists short of a private/undocumented API, which the standalone rule
and App Store distribution both forbid. No TCC entry, no file, no network.

## Vault mapping

- **Raw layer:** none (out of scope — vault path `""`).
- **Contract layer:** none. Pressure that *is* captured lands in the existing
  grandfathered `weather/` stream (`pressure_msl` from Open-Meteo), and Apple
  Watch `atmospheric_pressure` samples arrive via the Apple Health export path
  into `health/` — neither needs this integration.

## Build plan

None — unavailable. The card renders greyed with the reason above. If Apple
ever exposes the sensor on macOS, reopen as a `Live`/`Periodic` `environment/`
collector; until then this stays a catalogued dead end so it is not
rediscovered and re-spiked.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Barometric pressure | 🚫 | n/a — blocked by platform; nothing to validate |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §macOS CoreMotion
barometric pressure (CMAltimeter) (L2200–L2207). Feasibility 🔴 blocked.
Two viable substitutes both already exist: Open-Meteo `pressure_msl` in the
weather stream, and Apple Watch `CMAltimeter` samples surfaced through the
HealthKit export (`atmospheric_pressure` in `export.xml`) for Watch owners —
neither adds new integration complexity, so there is no ROI in chasing the
Mac sensor.
