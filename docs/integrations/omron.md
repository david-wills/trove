# Omron

- **id:** `omron`
- **domains:** `health/` (readings reach the vault via Apple Health import; no
  separate Omron vault path is written by this entry)
- **status:** 🚫 unavailable
- **unavailable_reason:** OMRON Connect Create API is B2B/partner-gated
  server-to-server password-grant OAuth, not available to independent personal
  apps; readings reach Trove via the OMRON Connect app -> Apple Health,
  captured by the Apple Health import.
- **behavior:** Unavailable — direct API is contact-gated B2B; data path is
  OMRON Connect app → Apple Health → Trove health import
- **connection:** none (no user-facing OAuth; B2B password-grant with
  partner-issued credentials only)
- **evidence:** official but contact-gated — OMRON Connect Create API at
  digitalhealth.omronconnect.com; community fallback `libomron`
  (github.com/openyou/libomron) is aging and covers older serial/BT
  models only
- **effort / priority:** M / P2
- **outcome:** Unavailable — OMRON Connect Create API is B2B/partner-gated (password-grant OAuth
  with partner credentials, not a user-facing authorization-code flow); direct API access is not
  available to independent personal applications. The OMRON Connect app already syncs all
  readings to Apple Health, which Trove's Apple Health import captures. No separate connector
  needed or buildable.

## What it is

Omron is the dominant consumer blood-pressure-monitor brand. Its OMRON
Connect Create API serves BP readings (systolic/diastolic/pulse) plus the
irregular-pulse flag. The catch: users of the OMRON Connect iOS app
already land their readings in Apple Health, so the shipped Apple Health
import captures most Omron data today — the direct API mainly adds value
for users who don't sync to Apple Health.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| BP readings | requires approved developer onboarding | systolic, diastolic, pulse, timestamp | official docs |
| Irregular-pulse flag | same | boolean flag per reading | official docs |
| Direct device read (libomron) | older serial/BT models, USB/HID | raw device memory | community lib (aging, low confidence) |

## Access & auth

- OMRON Connect Create API: server-to-server password-grant OAuth 2.0 at
  digitalhealth.omronconnect.com. Developer onboarding requires contacting
  Omron (not self-service) and is gated on B2B partner approval; not
  available to independent personal applications.
- A Device SDK exists for direct BLE pairing — requires the app running
  against hardware; violates the standalone posture for sync, skip.
- No TCC, no local files for the API path. Standalone-clean (plain HTTPS).

## Build plan (historical — outcome: Unavailable)

1. **Spike first** (per the research recommendation): apply for OMRON
   Connect Create onboarding and document the outcome; simultaneously
   measure how much Omron data the Apple Health import already captures.
2. Onboarding was found to be contact-gated and partner-only (B2B
   password-grant, not a user-facing authorization-code flow). Step 4
   applies: status set to 🚫 unavailable.
3. All Omron readings already reach the vault through the Apple Health
   export.zip import (OMRON Connect app → Apple Health → Trove).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Apple Health coverage | confirmed | Enable the Apple Health import; OMRON Connect app users find their BP readings in health/blood-pressure/ |

## Research notes

`integrations-research.md` → Health: Wearables & Biometrics §Omron Blood
Pressure Monitors (L910–L916). Feasibility 🟡 medium, entirely on the
onboarding gate. Outcome: onboarding is B2B/partner-gated; direct connector
not buildable for independent apps. Withings (BPM Connect cuff) is the
API-friendly alternative for BP.
