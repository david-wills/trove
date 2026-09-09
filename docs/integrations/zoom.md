# Zoom

- **id:** `zoom`
- **domains:** `meetings/` (contract: **Phase 3 pending** — drafted from
  Granola + Fathom + Zoom + Fireflies together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (cloud-recordings poll) + a local-folder scan of
  `~/Documents/Zoom/` in the same def's pull — one provider, two mechanisms,
  one card
- **connection:** `zoom` — OAuth (user-level app, `recording:read` scope).
  Not shared with other defs. Local-recordings scan works with **no**
  connection — the card must not gate the local path behind login.
- **evidence:** official-docs — Zoom REST API v2 (`GET /users/me/recordings`,
  VTT transcript files with download tokens); local WebVTT files in
  `~/Documents/Zoom/` are a documented, standard format
- **effort / priority:** M / P1
- **needs:** privacy (meeting content ≈ message bodies — opt-in with explicit
  acknowledgement) · Needs-login (OAuth app credentials + a Pro account for
  the cloud slice) · meetings contract not yet ratified (Needs-David)

## What it is

The dominant work meeting platform. Two distinct mechanisms, one entry:
cloud recordings + VTT transcripts + AI Companion summaries via the REST
API (Pro plan, host-enabled), and local recordings (`.mp4`/`.m4a`/`.txt`
chat/`.vtt` transcript) saved to `~/Documents/Zoom/` with no plan or
permission requirements. The local path catches meetings the cloud path
misses and vice versa.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Cloud recording list | Pro+ (cloud recording is a paid feature) | meeting topic, time, duration, UUID | official docs |
| Cloud VTT transcript | Pro+ AND host enabled "Audio Transcript" pre-meeting (off by default) | timestamped speaker utterances | official docs |
| AI Companion summary | account has AI Companion AND user was host (non-host access is a known open issue, 2026) | summary text | official docs |
| Local recordings | any plan, no auth | meeting dir name + date, chat .txt, .vtt if "Save closed caption as VTT" enabled | research doc (standard files) |

All optional in the contract; a free-plan user gets local-only rows, no
special code paths.

## Access & auth

- Cloud: REST v2 at `https://api.zoom.us/v2/`; `GET /users/me/recordings`
  (date-ranged). Each `recording_files` entry with `file_type='TRANSCRIPT'`
  carries a `download_url` + `download_access_token`. OAuth 2.0 user-level
  app, scope `recording:read`. Download tokens expire in 24h — never
  persist them; the list endpoint re-issues.
- Local: scan `~/Documents/Zoom/<meeting-name + datestamp>/` for new
  directories. Plain user files — no TCC prompt needed for Documents in the
  app's normal grant model (verify in the loop; Documents access may prompt
  once on macOS).
- Standalone-clean: HTTPS + local file parsing; VTT is standard WebVTT.

## Vault mapping

- **Raw layer:** `meetings/zoom/raw/YYYY-MM.jsonl` (API recording objects) +
  VTT/chat artifacts copied under `meetings/zoom/raw/files/` keyed by
  meeting UUID.
- **Contract layer:** `meetings/zoom/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per meeting (`ts`, `source`, `guid` = meeting
  UUID, `title`, `duration_secs`, `summary` when AI Companion yields one),
  transcript as sidecar document, overflow in `extra`. Cloud and local
  mechanisms write the **same** path.
- **Dedupe:** meeting UUID as `guid` (present in the API object and in the
  VTT metadata line / directory name) — this is what merges the two
  mechanisms. Cursor + seen-dirs set in `.trove/zoom-sync.json`,
  rebuildable.

## Build plan

1. Module `crates/trove-core/src/zoom.rs`: `DEF` (Periodic; pull = cloud
   poll when connected + local scan always), `CONNECTION` (OAuth;
   `recording:read`), Sync-now hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. WebVTT parser (shared candidate — Webex/Teams/Meet also emit VTT; put it
   somewhere reusable, not zoom-private).
4. Fixtures: API recording-list response (with TRANSCRIPT file entry), a
   sample local meeting dir (VTT + chat .txt), a UUID-collision case
   proving cloud+local dedupe to one row.
5. Privacy gate: ships opt-in (transcripts are conversation content) —
   explicit acknowledgement on enable. Disabled-state affordance on the
   connect card per the SimpleFIN rule; local scan stays usable without
   OAuth and the card copy must say so.
6. Parked behind the meetings contract (Needs-David).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local recordings | — | record a test meeting locally with "Save closed caption as VTT" on; Sync now; row + transcript sidecar in `meetings/zoom/` |
| Cloud list + VTT | — | needs a Pro account with cloud recording + Audio Transcript enabled; OAuth connect; Sync now; confirm rows |
| AI Companion summary | — | host a meeting on an AI-Companion-enabled account; confirm summary lands; confirm non-host meetings degrade gracefully (no summary, no error) |
| Cloud+local dedupe | — | record the same meeting both ways; confirm one row |

## Build notes (2026-06-16)

**Implemented:** `crates/trove-core/src/zoom.rs` — full Periodic collector.

**Cloud path:** OAuth 2.0 (`recording:read`), `GET /users/me/recordings` with
30-day sliding window cursor (`.trove/zoom-sync.json`). Paginates via
`next_page_token` until absent. Downloads TRANSCRIPT (VTT) files using the
per-file `download_access_token` (never persisted). VTT parsed into utterances
via `parse_vtt()` (standard WebVTT with `<v Speaker>` cue tags). Transcripts
stored as JSONL sidecars under `meetings/zoom/raw/files/<uuid>.jsonl`; raw VTT
also preserved as `<uuid>.vtt`. AI Companion summaries deferred — the endpoint
requires a separate call that is gated to hosts on AI-Companion-enabled accounts;
not implemented to avoid adding complexity for a feature most users won't have.

**Local path:** scans `~/Documents/Zoom/` for new meeting directories using a
seen-dirs set in the cursor. Parses directory datestamp for `ts`, `<v>` VTT
cues for the transcript sidecar, `.txt` chat log preserved in raw/files. If the
VTT `NOTE meetingId:` header is present, the real UUID is used as guid (enabling
cloud+local dedup); otherwise a stable `local-<YYYYMMDDHHMMSS>` key is used.

**Connection:** new OAuth `ConnectionDef` (id=`"zoom"`, `recording:read`,
redirect_port=38672). The integrator must add `&crate::zoom::CONNECTION,` to
`CONNECTIONS` in `integrations.rs`.

**Tests:** 24 tests — VTT parser, datestamp extraction, cloud mapping, cloud
pull (writes/dedup/pagination/graceful-no-transcript/cursor-advance), serde
back-compat, port assertion.

**Flags:** Needs-login (register a Zoom OAuth app), Needs-sample (no live Pro
account to validate cloud path), privacy-opt-in (default_on: false).

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Zoom
Cloud Recordings & Transcripts (L514–L521) + §Zoom Local Recordings
(L570–L577). Feasibility 🟢 high, both mechanisms. Gotchas carried forward:
transcript only exists when the host enabled the setting pre-meeting; AI
Companion summaries are host-only via user-level OAuth (open developer
forum issue as of 2026); download tokens expire in 24h. The local path is
the zero-friction on-ramp — it works for free-plan users and needs no
OAuth app review.
