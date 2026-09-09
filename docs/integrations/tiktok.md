# TikTok

- **id:** `tiktok`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/ad-interests stay per-source raw under `social/tiktok/`),
  `media/plays/` (✅ ratified — video browsing history),
  `correspondence/` (✅ ratified — DMs)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (official data export ZIP, JSON variant)
- **connection:** none (export requested in-app; no credential touches
  Trove)
- **evidence:** official export flow (Settings and privacy → Account →
  Download your data, **JSON** format); official Data Portability API
  exists at developers.tiktok.com (developer registration — deferred)
- **effort / priority:** S / P2
- **needs:** privacy (DM bodies → opt-in with explicit acknowledgement
  for the correspondence slice) · time-sensitive UX copy (download link
  valid only days — import promptly)

## What it is

Short-form video. The headline asset isn't the user's posts — it's the
**watch history**: 10k–50k+ timestamped video views for active users, one
of the densest attention records any platform exports. DMs and liked
videos ride along in the same ZIP.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Video browsing history | all accounts | watch events w/ ts + video URL | official export, Activity/ (L4036–L4038) |
| Liked videos / comments / shares | all accounts | ts, video ref, comment text | Activity/ folders |
| DMs | all accounts | thread, ts, body | Direct Messages/ |
| Ad interests | all accounts | inferred interest labels | Ads and Data/ |
| Own posts | all accounts | post metadata | export categories |

All optional in the contract. One ZIP, several folders — the importer
routes by shape, never splits a record.

## Access & auth

- User flow: Profile → Settings and privacy → Account → Download your
  data → **select JSON, not TXT** (TXT is far less complete — the setup
  copy must say this). Ready in 1–4 days typically (up to 30 per policy).
- **Download link is valid for only a few days** after the ready
  notification — the def's copy tells the user to import promptly.
- Data Portability API (M5, developer registration) deferred — spike
  later for incremental sync; the archive covers history resiliently.
- No TCC, no network at import time. Standalone-clean.

## Vault mapping

- **Raw layer:** `social/tiktok/raw/` — the export JSON as parsed, full
  fidelity; likes/comments/shares/ad-interests stay per-source raw here.
- **Contract layer:**
  - watch events → `media/plays/tiktok/YYYY-MM.jsonl` per the ratified
    media-plays contract (`ts`, `source`, `guid` = hash(ts + video URL —
    the export has no event id), `kind` = video, URL in the link field);
  - DMs → `correspondence/tiktok/` per the ratified correspondence
    contract (thread as conversation handle);
  - own posts → `social/tiktok/` per the pending social-posts contract.
- **Dedupe:** deterministic hashes where the export lacks ids — repeat
  imports of overlapping archives merge cleanly.

## Build plan

1. Module `crates/trove-core/src/tiktok.rs`: `DEF` (Import; setup copy:
   JSON-not-TXT, link-expiry warning). ✅ DONE
2. One registration line in `INTEGRATIONS`. No `CONNECTION`. ✅ (stub registration already existed)
3. ZIP walker over `user_data.json`; JSON field names confirmed from community
   analysis (rothgar nushell tiktok-download gist, extratone gist):
   `Activity.Video Browse History.VideoList[{Date, Link}]`,
   `Direct Messages.ChatHistory.ChatHistory.{contact: [{Date,From,To,Content,MediaType}]}`,
   etc. Parsers tolerant; Needs-sample flag set for real-export verification. ✅ DONE
4. Privacy gate: DM slice opt-in via `acknowledge_dms` param; watch
   history, likes, ad interests, posts import without it. ✅ DONE
5. media-plays + correspondence writes via existing `store` helpers (both
   ratified). social-posts rows raw-only pending Phase 3. ✅ DONE
6. 12 tests green; cargo check green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Watch history | ✅ built (Needs-sample) | request a real export (JSON); import; confirm volume-appropriate rows in `media/plays/tiktok/` + hub last-data |
| DMs | ✅ built (opt-in, Needs-sample) | same ZIP; type 'yes' in acknowledge_dms field; confirm `correspondence/tiktok/` rows |
| Likes/ads/posts | ✅ built (raw only) | confirm per-source raw files under `social/tiktok/raw/`; re-import same ZIP → zero duplicates |

## Research notes

`integrations-research.md` → Social Media §TikTok (L4032–L4038).
Feasibility 🟢 high for import. US legal status of TikTok has been
uncertain; as of June 2026 the app is operational — archive import is
deliberately resilient to platform shutdown (another reason import-first
beats the API here). Catalog cross-cutting note: GDPR archive export is
the bedrock path across all closed social platforms.
