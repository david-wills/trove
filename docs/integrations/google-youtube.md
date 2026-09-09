# YouTube

- **id:** `google-youtube`
- **domains:** `youtube/` (grandfathered raw snapshots — shipped def;
  closed-set path, do not extend) · `media/plays/` (contract: ✅ ratified —
  watch history, delivered via the **google-takeout** importer) ·
  `browser/searches/` (raw-only — YouTube search history, same Takeout
  archive) · `social/youtube/` (raw — creator analytics extension; the
  social-posts contract is Phase 3 pending and doesn't cover analytics)
- **status:** 🧪 built (API side shipped pre-pipeline as the `google-youtube`
  def; extensions queued)
- **unavailable_reason:** none
- **behavior:** Periodic (Data API polls on the shared Google connection);
  watch-history backfill is Import via the google-takeout provider
- **connection:** `google` — OAuth, shared with the five other google-*
  defs (one login, full-scope bundle, multi-account by `sub`)
- **evidence:** official API docs (Data API v3 — shipped); official Takeout
  (`watch-history.json`, JSON format) via google-takeout-parser-documented
  schema; YouTube Analytics/Reporting API officially documented
- **effort / priority:** M / P1
- **needs:** extension — Takeout watch-history import (watch history has
  been API-impossible since 2016/2017; coordinate with the `google-takeout`
  brief, which owns the importer) · extension — creator analytics
  (Analytics/Reporting API or Studio CSV)

## What it is

The user's video life: subscriptions, playlists, liked videos, and — the
crown jewel — complete watch history back to account creation. The Data
API covers the curation surfaces; watch history exists **only** in Takeout.
Creators additionally have channel analytics. One Google login serves all
of it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Subscriptions / playlists / liked videos | none; 10k units/day quota (generous) | channel + video + playlist metadata (LL pseudo-playlist for likes) | official docs — **shipped** |
| Watch history (Takeout) | none | title, titleUrl (videoId), channel, ISO 8601 time; no duration/percentage | google-takeout-parser schema |
| Search history (Takeout) | none | query, time | google-takeout-parser schema |
| Creator analytics | channel owners only | per-video/channel metrics (Analytics API; Reporting API bulk jobs; Studio CSV ≤500 rows/view) | official docs |

All optional; non-creators simply never produce analytics rows.

## Access & auth

- **Shipped:** Data API v3 (`playlistItems?playlistId=LL`, `/subscriptions`,
  `/playlists`) on the `google` OAuth connection, scope `youtube.readonly`.
- **Watch/search history:** Takeout archive only — `history/watch-history.json`
  (must select JSON format) and search activity. The `google-takeout`
  provider's import box handles the archive; see that brief for the Data
  Portability API upgrade path (BYO credentials).
- **Creator analytics:** YouTube Analytics API rides the same `google`
  OAuth (additional scope); Reporting API for bulk dumps.
- No TCC. Standalone-clean (HTTPS to Google by explicit user choice).

## Vault mapping

- **Raw layer:** `youtube/` — grandfathered snapshot path the shipped def
  already writes (subscriptions/playlists/likes); stays as-is until the
  post-wave tidy-up migration. Creator analytics → `social/youtube/`
  (per-source raw). Takeout-delivered slices land under the
  google-takeout source folders (`media/plays/google-takeout/`,
  `browser/searches/google-takeout/`) per that brief.
- **Contract layer:** watch events normalize into the ratified media-plays
  contract (handled by the google-takeout importer: kind = video, channel
  as artist-equivalent, videoId in `extra`). Curation snapshots stay
  raw-only per the media-curation rule.
- **Dedupe:** API rows by native ids; Takeout rows `guid` =
  hash(time, titleUrl) — ads filtered (keep only `/watch` + `/shorts`
  titleUrls).

## Build plan

1. **Shipped:** `google-youtube` def (Periodic, `google` connection) —
   nothing to redo.
2. Extension A (owned by `google-takeout`'s loop iteration): watch/search
   history import; this brief just records the dependency — when it ships,
   YouTube watch history is covered with zero work here.
3. Extension B (this provider): creator analytics pull — add an
   analytics-scope slice on the `google` connection, write
   `social/youtube/`; Reporting API only if Studio-CSV-scale proves
   insufficient. Fixtures from official example responses.
4. Read-time idea (not a write path): enrich Takeout watch rows with Data
   API video metadata.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Subscriptions/playlists/likes | 🧪 shipped pre-pipeline — David promotes to ✅ | Google connect card → Sync now; confirm `youtube/` snapshots + hub last-data |
| Watch history (Takeout) | — | via google-takeout validation: import a real archive, confirm `media/plays/google-takeout/` rows |
| Creator analytics | — | needs a channel-owning account; pull a month of metrics, confirm `social/youtube/` rows |

## Research notes

`integrations-research.md` → "Web Activity" §YouTube Data API v3
(L1544–L1551, 🟢 high), "Media" §YouTube watch history (L3302–L3308, 🟢
high), "Social" §YouTube as creator (L4096–L4102, 🟢 high for Takeout).
Watch history was removed from the API in 2016/2017 — Takeout is the only
route; export must switch History from HTML to **JSON**. Ads appear in
watch-history.json and must be filtered. Overlap is three-way by design:
this def (API curation), google-takeout (history import), and creator
analytics — one provider entry in the hub, one `google` login.
