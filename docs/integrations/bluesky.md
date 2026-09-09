# Bluesky

- **id:** `bluesky`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/follows/profile snapshots stay per-source raw under
  `social/bluesky/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (incremental API poll) + one-shot full-history
  backfill via the unauthenticated CAR repo export
- **connection:** `bluesky` — TokenPaste (App Password from Bluesky
  settings) for the authenticated incremental pull; OAuth 2 PKCE (live
  since Sep 2024) is a possible later upgrade. The historical CAR backfill
  needs **no credentials at all**. Not shared with other defs.
- **evidence:** official-docs — atproto.com / AT Protocol XRPC endpoints
  (`com.atproto.sync.getRepo`, `app.bsky.feed.getAuthorFeed`,
  `app.bsky.feed.getLikes`, `app.bsky.graph.getFollowers`); research
  feasibility 🟢 high ("best-in-class open social API")
- **effort / priority:** M / P2
- **needs:** none

## What it is

Open-protocol microblogging on AT Protocol. The user's entire repo —
posts, likes, follows — is a content-addressed archive that anyone can
fetch by DID with **no auth, no API key, no rate-limit or cost barrier**.
The cleanest social integration in the catalog: full history in one shot,
incremental sync fully documented, no app-review gate.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full post/like/follow history | none (public repo) | all repo records (DAG-CBOR) | official atproto docs — CAR export |
| Incremental posts | none | author feed entries | official docs — `getAuthorFeed` |
| Likes | none | liked-post refs | official docs — `getLikes` |
| Followers/following | none | follow graph entries | official docs — `getFollowers` |

All optional in the contract. DMs are **not** included — Bluesky's DM
layer is separate from the public repo and not exposed here; the brief
covers public-repo data only (say so in the hub copy).

## Access & auth

- Backfill: `GET https://<pds-host>/xrpc/com.atproto.sync.getRepo?did=<did>`
  returns a CAR file (Content Addressable aRchive, DAG-CBOR records).
  Unauthenticated — the user supplies only their handle/DID.
- Incremental: XRPC endpoints (`getAuthorFeed`, `getLikes`,
  `getFollowers`, …) with an App Password session or OAuth 2 token.
- Parse CAR with the `iroh-car` or `atrium` Rust crates (per the research
  doc) — compiled-in libraries, standalone-clean. Plain HTTPS, no TCC.

## Vault mapping

- **Raw layer:** `social/bluesky/raw/` — decoded repo records, full
  fidelity; likes/follows/profile snapshots stay here per the taxonomy
  (per-source raw; never routed to `reading/`).
- **Contract layer:** `social/bluesky/YYYY-MM.jsonl` once the Phase 3
  social-posts contract is ratified — expected one row per post (`ts`,
  `source`, `guid` = record CID/rkey, text, reply/quote refs, media
  metadata), overflow in `extra`.
- **Dedupe:** record key (CID) as `guid` — content-addressed, ideal;
  incremental cursor in `.trove/bluesky-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/bluesky.rs`: `DEF` (Periodic) +
   `CONNECTION` (TokenPaste: App Password, with handle/DID field); first
   run does the CAR backfill, subsequent runs poll incrementally.
2. CAR/DAG-CBOR decoding via `atrium`/`iroh-car` (evaluate at build time;
   both flagged in research).
3. One line each in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures: a small real-shape CAR slice + API feed JSON; parser, cursor,
   dedupe, store tests with unique temp dirs.
5. Contract rows parked behind the Phase 3 social-posts contract
   (raw layer can ship first — full fidelity first, normalization second).

## Build notes (2026-06-17)

- **CAR backfill NOT built**: the brief mentions a CAR/DAG-CBOR backfill path, but `iroh-car`/`atrium` are heavy crates (DAG-CBOR, IPLD) with no existing usage in the codebase. The AppView REST `getAuthorFeed` API provides the same full history (oldest-first via cursor drain) with no extra deps. CAR decode deferred as Needs-sample / future enhancement.
- **Auth**: `handle:app-password` → `createSession` → accessJwt stored in `.trove/sync/bluesky-token.json`. App Password itself never persisted.
- **Endpoint**: `app.bsky.feed.getAuthorFeed` on `public.api.bsky.app` (Bluesky AppView). Auth required for user's own feed.
- **Contract**: `social/bluesky/YYYY-MM.jsonl` (Post rows, `guid = post.uri` AT-URI). Raw: `social/bluesky/raw/YYYY-MM.jsonl` (full FeedViewPost items).
- **Cursor**: `.trove/bluesky-sync.json`, non-secret. First sync drains to empty; incremental stops when all items on a page are already held.
- **No new cargo deps**: uses existing ureq + serde_json.
- **CONNECTIONS line**: `&crate::bluesky::CONNECTION,` must be added to `CONNECTIONS` in `integrations.rs` by the integrator.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Full post history backfill | 🧪 ready | Paste handle:app-password; first Sync now; confirm posts in `social/bluesky/YYYY-MM.jsonl` |
| Incremental pull | 🧪 ready | Post on Bluesky; Sync now again; confirm new row + hub last-data |
| Dedupe on re-run | 🧪 ready | Sync now twice; confirm no duplicate guids in vault |
| Likes/follows | deferred | Stored as raw FeedViewPost items only; not authored content |
| CAR backfill | deferred | REST feed covers full history; CAR parse Needs-sample |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Bluesky
(L3984–L3990). Feasibility 🟢 high. Public repo only — no DMs. No paid
tier, no app review, no key distribution problem. Not time-sensitive (the
repo is always fetchable). Mastodon is the sibling open-protocol provider;
the two should exercise the social-posts contract together.
