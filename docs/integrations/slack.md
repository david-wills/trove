# Slack

- **id:** `slack`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (export import shipped pre-pipeline in `slack.rs`; David
  promotes to ✅ after real-data validation)
- **unavailable_reason:** none
- **behavior:** Import (workspace export ZIP, shipped) · planned extension:
  Periodic API pull for live DMs + private channels
- **connection:** none today; the API-pull extension adds a `slack` connection
  (ConnectMethod::OAuth with compiled-in app creds, plus TokenPaste for a
  bring-your-own `xoxp-` user token)
- **evidence:** official workspace-export ZIP format (shipped, fixture-tested);
  official API docs — `conversations.list` / `conversations.history` /
  `users.info`, stable non-expiring user tokens
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (message bodies — import is user-initiated and
  the API pull ships opt-in with explicit acknowledgement)

## What it is

The dominant team-chat platform; for many users the bulk of their work
conversations live nowhere else. Catalog flags it time-sensitive: history on
free workspaces gets pruned by plan limits, so capturing an export early
preserves what later exports won't contain.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Public-channel history (export ZIP) | all plans | text, sender, ts, channel, reactions, attachments meta | shipped slack.rs |
| DMs + private channels (export ZIP) | Business+ / admin compliance export only | same | official export docs |
| DMs + private channels (API pull) | any plan, user token | same, incremental via `ts` cursor | official API docs |

All optional in the contract; a free-plan export simply yields no DM rows
until the API extension lands. No tier-specific code paths.

## Access & auth

- **Export (shipped):** Workspace Settings → Import/Export → Export → ZIP of
  JSON per channel+date. No auth on the Trove side; user supplies their Slack
  handle so own messages are marked `from_me`.
- **API extension (planned):** OAuth 2.0 user token (`xoxp-`), scopes
  `channels:history`, `im:history`, `mpim:history`, `groups:history`,
  `channels:read`, `im:read`, `users:read`. Rate limit Tier 3 ≈ 50 req/min
  per workspace — fine for a personal periodic pull. Tokens don't expire by
  default.
- No TCC. Standalone-clean (ZIP parse + plain HTTPS).

## Vault mapping

- **Raw layer:** none needed for the export (the ZIP itself is the user's raw
  copy); the API pull may keep `correspondence/slack/raw/` if responses carry
  fields beyond the contract.
- **Contract layer:** `correspondence/slack/YYYY-MM.jsonl` per the ratified
  correspondence contract — `guid` = `channel/ts`, `chat` = channel id,
  `chat_name` = channel name, `service` = workspace, reactions as
  `kind:"reaction"`. Mentions resolved to names at import time (the id→name
  map only exists inside the ZIP).
- **Dedupe:** `guid`; API watermarks per channel in
  `.trove/sync/slack-watermarks.json`, rebuildable from output files.

## Build plan

1. Already shipped: `slack.rs` Import def, registered, generic import box.
2. Extension iteration: add `CONNECTION` (`slack`, OAuth + TokenPaste BYO),
   flip nothing in the importer — the pull writes the same sink with the same
   guids, so export + API runs interleave safely.
3. Fixtures: existing export fixtures + `conversations.history` example
   responses; watermark cursor tests, unique temp dirs.
4. UI copy already documents the free-plan export gap (caveats string);
   keep it and point to the API pull once it exists.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Export import | 🧪 shipped pre-pipeline | import a real workspace export ZIP; confirm rows in `correspondence/slack/` + hub last-data; re-import dedupes |
| API pull (DMs/private) | — | connect a real workspace token; Sync now; confirm DM rows the export lacked |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Slack workspace export
(L249-255) + §Slack API pull (L257-263). Feasibility 🟢 high on both paths.
Free/Pro exports cover public channels only — the API pull is the documented
fix for the DM gap on any plan, same JSONL sink. Time-sensitive: export early,
before plan-limit pruning.
