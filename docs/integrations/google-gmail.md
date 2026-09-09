# Gmail

- **id:** `google-gmail`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (shipped pre-pipeline as part of the Google entity;
  David promotes to ✅ after real-data review)
- **unavailable_reason:** none
- **behavior:** Periodic (full-history backfill, then `historyId` incremental)
- **connection:** `google` — OAuth (shared with google-calendar,
  google-contacts, google-tasks, google-youtube, google-books; one login,
  six defs)
- **evidence:** official-docs — gmail.googleapis.com REST
  (`users/me/messages` list + get, `format=raw`, `historyId` cursor)
- **effort / priority:** M / P0
- **needs:** privacy-sensitive (message bodies — opt-in with explicit
  acknowledgement)

## What it is

The world's largest consumer email service. Email is the densest single
correspondence source most users have — decades of conversations,
receipts, confirmations, and relationships. The API pull gives live
incremental sync; Takeout `.mbox` backfills bulk history through the
existing `email` importer, and both land dedupe-safe in the same sink.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full message history (backfill) | all accounts | RFC822 payload → ts, chat, sender, text, to, subject, attachments meta | official docs |
| Incremental new mail | all accounts | same shape via `historyId` delta | official docs |
| Labels | all accounts | `labels[]` (INBOX, CATEGORY_*, user labels) | official docs |

All optional in the contract; no tier-specific code paths (Gmail has no
relevant plan gating for read access).

## Access & auth

- REST: `https://gmail.googleapis.com/gmail/v1/users/me/messages` — list +
  get with `format=raw` (verbatim RFC822 bytes, so the same parser as the
  mbox importer). Scope `gmail.readonly`, part of the Google full-scope
  bundle consent.
- Cursor: `users.getProfile` `historyId` captured before backfill page 1;
  `history.list` after baseline yields only newly-added messages.
- Rate limit: 250 quota units/sec/user — fine for a personal pull.
- Baked + BYO client_id per ConnectSpec; no TCC; standalone-clean HTTPS.

## Vault mapping

- **Raw layer:** none separate — the RFC822 payload is decoded straight to
  contract rows (the `format=raw` bytes are the full-fidelity record;
  re-parse risk is covered by re-runnable backfill).
- **Contract layer:** `correspondence/email/YYYY-MM.jsonl` — the shared
  email sink (source `email`), per the ratified correspondence contract.
  Message-ID as `guid` makes the API pull and the mbox import mutually
  dedupe-safe: overlapping Takeout + live sync never duplicate.
- **Cursor:** `historyId` in sync state, rebuildable.

## Build plan

Shipped (`gmail.rs`, registered in `INTEGRATIONS`; `google` connection in
`sync/google.rs`). Remaining work is pipeline-conformance only:

1. Confirm the brief's contract mapping matches the shipped writer (it
   does — `vault_path: "correspondence/email/"`).
2. No new module work unless the Phase 4 loop's verify step finds drift
   against live docs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backfill + labels | 🧪 built (pre-pipeline) | connect Google, Sync now, confirm history lands in `correspondence/email/` with labels; David promotes to ✅ |
| `historyId` incremental | 🧪 built (pre-pipeline) | send self a test mail, Sync now, confirm exactly one new row + cursor advance |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Gmail (L209-L215).
Feasibility 🟢 high — API fully alive in 2026. Takeout exports one `.mbox`
per label; the existing `email` importer handles those for users who
prefer never granting OAuth. Google OAuth app registration satisfied by
the baked-creds + BYO-client_id ConnectSpec model.
