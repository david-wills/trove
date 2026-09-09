# Twitch

- **id:** `twitch`
- **domains:** `social/twitch/` (per-source raw; social-posts contract is
  **Phase 3 pending** — chat/clips/channel data is mostly per-source raw)
- **status:** 🧪 built (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (official data download ZIP; a Helix-API Periodic
  def for creator analytics is a possible later addition)
- **connection:** none for the import. A future Helix path would add a
  `twitch` OAuth connection — recorded, not in scope for the first build.
- **evidence:** official download exists (Settings > Security and Privacy >
  Download Your Data) but Twitch publishes no breakdown of its contents —
  **sample-required**; Helix API is official-docs level for the later
  creator-analytics slice
- **effort / priority:** M / P2
- **needs:** Needs-sample (download contents undocumented; parser-last)

## What it is

Live-streaming platform. For streamers, the account data and Helix API
carry channel/creator history (clips, videos, subscriptions, analytics);
for viewers, the data export includes a **Viewing and Chat History** category
with a `_minutes_watched.csv` (minutes watched per channel). The Helix API
does NOT expose viewer watch history — but the data export does. Mainly
relevant to people who stream or chat heavily.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Personal data download | all accounts | account data; file inventory known: `_minutes_watched.csv` (viewing), `_follow_unfollow.csv`, `_chat_cheer_sub_notif.csv` | official flow + community research (jakelee.co.uk, unfollowertrackers.com); column layout unconfirmed — sample required |
| Creator channel data (Helix, later) | streamers; OAuth | channels, clips, videos, subscriptions | official Helix docs |
| Viewer watch history (Helix API) | — | **not available via the Helix API** | Helix API docs confirm absence |
| Viewer watch history (data export) | all accounts | minutes watched per channel (`_minutes_watched.csv`) — routes to media-plays once column layout confirmed | community research; official Twitch export "Viewing and Chat History" category |

The distinction between API and export is important: absence from the Helix
API does not mean absence from the export package. The export watch data
will route to the media-plays contract once a real sample confirms column names.

## Access & auth

- Data download: twitch.tv/privacy/controls (Settings > Security and
  Privacy > Download Your Data); processing can take up to 30 days; package
  format/timeline not documented publicly.
- Helix API (later): OAuth Authorization Code flow; GET /channels, /clips,
  /videos, /subscriptions. Not part of the first import build.
- No TCC; import path makes no network calls. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/twitch/raw/` — the download package verbatim,
  whatever it turns out to contain.
- **Contract layer:** file inventory is known from community research;
  routing awaits column confirmation from a real sample. Expected:
  `_minutes_watched.csv` → media-plays contract; `_follow_unfollow.csv` +
  `_chat_cheer_sub_notif.csv` → social contract (correspondence-shaped).
- **Dedupe:** `guid` from package record ids/timestamps — decided against
  the sample.

## Build plan

1. **Parser-last** (Needs-sample): request a real download first; the
   parser is written against what actually arrives, not guesses.
2. Module `crates/trove-core/src/twitch.rs`: `DEF` (Import), import hook;
   one line in `INTEGRATIONS`. No `CONNECTION` in the first build.
3. Import-box copy: set the up-to-30-day expectation; distinguish that the
   Helix API has no viewer watch history but the export DOES include
   minutes-watched data (routes to media-plays once column layout confirmed).
4. Later (separate loop iteration, recorded here): Helix OAuth connection +
   Periodic def for creator analytics, for users with channels.
5. Until a sample exists, ship as a NotWired/planned stub.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Data-download import | ✅ raw scaffold live | drop any file on the import box; confirm `social/twitch/raw/<stamp>-<name>` verbatim + hub last-data shows stamp |
| Per-item parse | ⏸ parked | request a real download from twitch.tv; enumerate contents; implement `posts_from_export` in `twitch.rs` |
| Contents inventory | ⏸ parked | request the download from a real account; update capability table and routing from observation |

## Build notes (2026-06-21)

- Upgraded from `NotWired` stub to `Behavior::Import` with raw-first scaffold.
- Raw layer unconditional: `social/twitch/raw/<stamp>-<filename>` accumulating snapshots.
- Contract layer parked: file inventory now known (`_minutes_watched.csv`, `_follow_unfollow.csv`,
  `_chat_cheer_sub_notif.csv`) from community research, but exact column names unconfirmed.
  Parser hook is `posts_from_export` in `twitch.rs`; raw layer already in place.
  Routing plan: minutes-watched → media-plays; chat/follows → social.
- No connection (import path makes no network calls; standalone-clean).
- 8 unit tests green; `cargo check` clean.
- The download can take up to 30 days to prepare — the hub card + setup copy makes this clear.
- Corrected (2026-06-21): Helix API has no viewer watch history, but the export DOES include
  `_minutes_watched.csv`; earlier build notes and caveats incorrectly stated the absence was
  universal. Caveats, setup copy, PARKED_MSG, and module docs updated to distinguish API vs. export.

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Twitch
(L4144–L4150). Feasibility 🟡 Medium — official mechanism, undocumented
contents. Third-party VOD-chat exporters (exportcomments.com) exist but are
external services — not a Trove path (standalone rule). Helix gives channel
analytics for streamers only; viewers get no watch history by design.
