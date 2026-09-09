# Netflix

- **id:** `netflix`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (official viewing-activity CSV; optional richer
  account-data ZIP as a follow-on)
- **connection:** none — the user downloads the export themselves while
  logged in at netflix.com; Trove never sees credentials.
- **evidence:** official-docs — netflix.com/account → Viewing activity →
  "Download all" instant CSV; "Request information about your account" ZIP
  ready in ~7–14 minutes
- **effort / priority:** S / P1
- **needs:** Needs-sample (the richer account-data ZIP only — its JSON
  shape is undocumented; the quick CSV is documented and built first)

## What it is

The largest streaming video service, and the first streaming import worth
building: the only major streamer with an *instant*, official viewing
history export. The quick CSV is sparse (title + date, per profile) but
free and immediate; the deeper "Request information" package adds duration
and device data within minutes. Backfills the pre-Trakt era for any film/TV
timeline; ongoing capture is Trakt's job (Netflix has no API).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Viewing activity (quick CSV) | any account, instant, per profile | Title, Date — episode names embedded in the title string | official export |
| Account-data ZIP | any account, ~7–14 min wait, link expires 72h | fuller viewing data incl. per-device + duration | official privacy request; format undocumented |

All optional in the contract; quick-CSV rows simply carry `seconds: 0`.
No tier-specific code paths.

## Access & auth

- Quick CSV: netflix.com/account → Privacy → Viewing activity → "Download
  all". One CSV **per profile** — the import box accepts multiple files
  and the user labels the profile (or accepts the filename).
- Richer ZIP: netflix.com → Privacy Settings → "Request information about
  your account" → select Viewing activity; download link expires in 72h —
  the in-app guide must say "import it promptly".
- No API, no TCC, no credentials handled. Standalone-clean (pure file
  import).

## Vault mapping

- **Raw layer:** `media/plays/netflix/raw/` — imported CSVs (and later ZIP
  contents) preserved as received.
- **Contract layer:** `media/plays/netflix/YYYY-MM.jsonl` per media-plays:
  `ts` = Date (date-only — normalize to local midnight; honest precision
  loss), `category:"video"`, `kind:"play"`, `title` = the full title
  string, `subtitle` = the series name parsed from the
  "Show: Season N: Episode" pattern (films: the title itself — `subtitle`
  is the chart grouping key), `seconds: 0` for quick-CSV rows; profile
  name in `extra.profile`.
- **Dedupe:** `guid` = hash of (profile, title, date) — the CSV has no id;
  re-imports of overlapping exports are idempotent.

## Build plan

1. Module `crates/trove-core/src/netflix.rs`: `DEF` (Import; registry-driven
   import box). `letterboxd.rs` is the reference import module.
2. Registration line in `INTEGRATIONS`.
3. CSV parser: two documented columns (Title, Date); series/episode split
   from the title string with a films-pass-through fallback; date formats
   vary by locale — parse defensively.
4. Richer-ZIP parser: **parser-last, Needs-sample** — undocumented format;
   wire the import box to accept the ZIP and stash it raw, ship the parser
   once a real sample is in hand.
5. Fixtures: hand-built CSVs covering films, multi-colon series titles,
   and locale date variants; parser + store tests, unique temp dirs.
6. Import-box copy: per-profile exports, and "for ongoing capture, connect
   Trakt and scrobble from your player".

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Quick CSV import | ✅ built | download a real profile's "Download all" CSV; drop on import box; row count matches the CSV; re-import adds nothing |
| Multi-profile | ✅ built | import two profiles' CSVs; confirm `extra.profile` distinguishes them |
| Account-data ZIP | — Needs-sample | blocked on a real sample; request one, import, confirm duration-bearing rows |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Netflix
(L3230–L3236). Feasibility 🟢 high; "dead simple M1 import… should be the
first streaming video import built". No public API exists or is expected.
Quick CSV has no duration or structured episode field — the title string
carries it (e.g. "Stranger Things: Season 1: Chapter 1"). The 72h link
expiry on the richer export is the only time-sensitivity. Disney+/Hulu/Max
and TV Time briefs point here as the pattern they *can't* follow.
