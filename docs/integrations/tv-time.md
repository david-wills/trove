# TV Time

- **id:** `tv-time`
- **domains:** `media/plays/` (contract: ✅ ratified media-plays — would
  apply if a path ever opens)
- **status:** 🚫 unavailable
- **unavailable_reason:** No official API or export. The only paths are
  fragile third-party Chrome extensions replaying TV Time's internal API —
  which Trove can't automate — or an unreliable GDPR request. Import your
  history into Simkl or Trakt instead.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** community only — Chrome extensions "TV Time Out" / "TV Time
  Liberator" replay the internal tvtime.com REST API client-side; GDPR
  request format undocumented and variable. No official docs, no sample.
- **effort / priority:** M / P2
- **needs:** none

## What it is

TV episode tracker (which series, which episode, when watched, watch
counts). Its history would map cleanly to `media/plays` video rows, but the
service offers no official API and no export, and its user base is
migrating to Trakt/Simkl anyway.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watch history (via extension) | free account | series, episode, season, watched date, watch counts (JSON/CSV) | community extensions reverse-engineering the internal API |
| Watch history (GDPR request) | free account | undocumented; format and completeness vary | research doc |

Neither is buildable: the extensions require the user to run Chrome,
authenticate, and export by hand (a browser-automation dependency Trove
won't take), and the internal API they replay can change without notice.

## Access & auth

No official API or export mechanism. The internal REST API at tvtime.com is
undocumented and only reachable as the logged-in browser session — exactly
the kind of fragile, automatable-only-via-browser path the standalone rule
excludes.

## Vault mapping

- **Raw layer:** none (nothing to write).
- **Contract layer:** if anything ever lands, rows would go to
  `media/plays/tv-time/YYYY-MM.jsonl` per the ratified media-plays contract
  (`category:"video"`). The supported route today: migrate history to Simkl
  (which imports from TV Time) or Trakt, then use those integrations.

## Build plan

None. Ship the `Behavior::Unavailable` stub with the reason copy and the
Simkl/Trakt migration pointer. A future revisit could accept the extension's
exported JSON/CSV as a hand-dropped import — but that format is folklore
(parser-last, Needs-sample) and the declining user base doesn't justify it
now.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed card with the reason copy and the Simkl/Trakt suggestion |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §TV Time
(L3374–L3380). Feasibility 🟡 medium-but-fragile; recommendation: icebox,
build Trakt and Simkl instead. The extensions extract client-side with no
server communication (privacy-fine, automation-impossible). TV Time is
losing users to Trakt/Simkl, which lowers the payoff of any future work.
