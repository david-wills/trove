# Noom

- **id:** `noom`
- **domains:** `health/nutrition/` (contract: **Phase 3 pending** — nutrition
  shape drafted from Cronometer + MyFitnessPal + MacroFactor)
- **status:** 🚫 unavailable
- **unavailable_reason:** Noom has no self-serve export or API — data arrives
  only via a GDPR/CCPA request (up to 30 days, undocumented format). Nutrition
  logged to Apple Health on iPhone is captured by the Apple Health export
  instead.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** Low — GDPR/CCPA request only (Settings → Manage Subscription →
  Request My Data, or gdprsupport@noom.com), undocumented delivery format, no
  API. Research doc feasibility 🟠 Low.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Weight-loss / behavioral-coaching app with meal logging. Catalogued so the hub
can answer "why isn't Noom available?" honestly: there is no self-serve data
path. Noom pivoted toward coaching; its nutrition logging is less complete
than dedicated trackers (food database licensed from MyNetDiary), and the only
access is a GDPR-style request with a 30-day wait and no documented format.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| GDPR data dump | any (30-day wait) | unknown — undocumented format, completeness unknown | research doc L1173 |
| Apple Health passthrough | iOS users | nutrition totals via export.zip (captured by the shipped `health` def) | research doc L1173, cross-cutting note 2 |

## Access & auth

No export UI, no API. GDPR/CCPA request via Settings → Account → Manage
Subscription → Request My Data, or email gdprsupport@noom.com; delivery within
30 days in an undocumented format. Not a buildable mechanism — there is
nothing to parse against and nothing to poll.

## Vault mapping

None — no integration is built. If a user's Noom data reaches Apple Health on
iPhone, it lands through the existing Apple Health import under `health/`
(no Noom-specific folder).

## Build plan

None. Revisit only if Noom ships a self-serve export or API, or if real users
supply GDPR dumps in a consistent format (which would re-open this as a
Needs-sample import). The app card renders greyed with the
`unavailable_reason` above, per `Behavior::Unavailable`.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows Noom dimmed in the Health group with the honest reason; "hide unavailable" filter removes it |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Noom (L1169–L1175); at-a-glance L1021; cross-cutting note 7
(blocked/low paths). Recommendation was icebox: users wanting structured
nutrition exports are better served by Cronometer or MyFitnessPal (both
queued). Catalogued as unavailable rather than omitted so the catalog stays
complete.
