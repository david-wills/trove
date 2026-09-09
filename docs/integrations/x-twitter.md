# X (Twitter)

- **id:** `x-twitter`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/followers/lists/ad data stay per-source raw under
  `social/x-twitter/`) · `correspondence/` (archive DMs — contract:
  **correspondence — ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (official account archive ZIP)
- **connection:** none — archive is user-initiated. API v2 is pay-per-use
  (no free personal tier worth shipping) — deliberately skipped; the hub
  copy says re-export to refresh.
- **evidence:** official archive mechanism (Settings → Your Account →
  Download an archive of your data); community-documented
  `window.YTD.<name>.part0 = […]` JS-wrapper format (strip the assignment
  prefix → valid JSON); research feasibility 🟢 high
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (DM message bodies — explicit opt-in
  acknowledgement at import time)

## What it is

The user's complete Twitter/X history in one official ZIP: full tweet
text (`full_text`, untruncated), media, likes, followers/following,
lists, ad engagements, and **full DM thread history** — often 15+ years
of posting and conversation. Since API v2 went pay-per-use, the archive
is the only practical path for general users, and it's a good one.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tweets | all accounts | `full_text`, media refs, URLs, timestamps | official archive (`tweet.js`) |
| DMs (1:1 + group) | all accounts | conversationId, senderId, text, createdAt, mediaUrls | archive (`direct-messages.js`, `direct-messages-group.js`) |
| Likes | all accounts | liked-tweet refs | archive (`like.js`) |
| Social graph | all accounts | follower/following lists | archive (`follower.js`, `following.js`) |
| Ad engagements | all accounts | ad-engagement records | archive (`ad-engagements.js`) |

All optional in the contract; sparse archives import fine.

## Access & auth

- Settings → Your Account → **Download an archive of your data** — ZIP
  with a `data/` folder of per-section `.js` files + HTML viewer.
  **Download link expires after 7 days** (export-link expiry, not data
  time-sensitivity — import promptly).
- Format quirk: files are JS modules (`window.YTD.tweets.part0 = [...]`).
  Strip the assignment prefix, then parse as JSON. Handle multiple
  `partN` files.
- Trove side: no auth, no network, no TCC. Standalone-clean.
- API explicitly skipped: $0.005/read pay-per-use (DM access effectively
  ~$100/mo tier) — not shippable as a default for general users.

## Vault mapping

- **Raw layer:** `social/x-twitter/raw/` — decoded archive sections;
  likes, follower/following lists, lists, ad data stay per-source raw
  here per the taxonomy.
- **Contract layer (posts):** `social/x-twitter/YYYY-MM.jsonl` once the
  Phase 3 social-posts contract is ratified — one row per tweet (`ts`,
  `source`, `guid` = tweet id, text, reply/retweet refs, media metadata),
  overflow in `extra`.
- **Contract layer (DMs):** `correspondence/x-twitter/YYYY-MM.jsonl` per
  the **ratified** correspondence contract — `kind:"message"`, `chat` =
  conversationId, `sender` = senderId, `from_me` matched against the
  archive owner's account id, `text`, media as attachment metadata,
  `service:"X"`.
- **Dedupe:** tweet id / DM message id as `guid` — re-imported (newer)
  archives overlap cleanly.

## Build plan

1. Module `crates/trove-core/src/x_twitter.rs`: `DEF` (Import); JS-wrapper
   stripper as a small shared helper; walk `data/`, route tweets →
   social, DMs → correspondence (records route whole, types split by
   shape).
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Fixtures: archive slice with multi-part tweets, a 1:1 and a group DM
   thread, likes; stripper/parser/dedupe/store tests, unique temp dirs.
4. Privacy acknowledgement (DM bodies) before first import.
5. Hub copy: archives are one-shot snapshots — prompt the user to
   re-export periodically; re-import dedupes.
6. Posts contract rows parked behind Phase 3 social-posts; DM rows ship
   against the ratified correspondence contract; raw layer ships first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tweet import | ✅ unit-tested | 11 unit tests; imports plain posts, replies, retweets to `social/x-twitter/YYYY-MM.jsonl` |
| DM import | ✅ unit-tested | 1:1 DMs → `correspondence/x-twitter/YYYY-MM.jsonl`; `from_me` resolved via `account.js` accountId |
| Wrapper stripping | ✅ unit-tested | `strip_js_wrapper` tested for normal, edge-case empty; no raw `window.YTD` strings in vault |
| Raw sections | ✅ unit-tested | likes/other sections → `social/x-twitter/raw/<section>.jsonl`, full fidelity |
| Dedupe | ✅ unit-tested | re-import is a no-op; tweet id / DM message id are the guid keys |
| Back-compat | ✅ unit-tested | sparse Post + Message lines still deserialize |

## Build notes

- **social contract**: `social` domain is ratified (Phase 3 complete, facebook.rs pioneer). The
  brief's note "posts parked behind Phase 3 social-posts" is stale — Post rows ship.
- **DMs correspondence contract**: ratified, ships per brief.
- Group DMs (`direct-messages-group.js`) parsed identically to 1:1 via same `dmConversation` shape.
- `account.js` owner detection is optional; absent → `from_me=false` for all DMs (safe default).
- Tweet timestamps use the old REST API format `%a %b %d %H:%M:%S %z %Y`; DM timestamps are ISO 8601.
- Raw tweets are written to `social/x-twitter/raw/tweets.jsonl` alongside Post contract rows.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §X (Twitter) DMs
(L369–L375) + "Social Media & Web Presence" §Twitter / X (L4000–L4006).
Feasibility 🟢 high for the archive; API path researched and rejected
(pay-per-use Feb 2026; "owned reads" at $0.001/read Apr 2026 still paid —
not a general-user default). Same import-ZIP pattern as the Meta
exports — shares the generic import-box flow.
