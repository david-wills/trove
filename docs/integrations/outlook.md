# Microsoft Outlook

- **id:** `outlook`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (fixture-tested; Graph multi-account OAuth; needs an Azure app + a real account to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (Graph API poll; deltaLink incremental cursor)
- **connection:** `microsoft` — OAuth (Microsoft Entra; compiled-in app
  registration + bring-your-own client_id fallback, PKCE public client).
  **Shared connection:** future Teams / To Do / OneNote / OneDrive defs ride
  the same login, Google-style — one entity, many defs.
- **evidence:** official-docs — Microsoft Graph v1.0 (`/me/messages`,
  `/me/mailFolders`, `/me/messages/{id}/$value` raw MIME; delta query for
  incremental sync). No public spec exists for the local OLM store
  (rejected mechanism — see notes).
- **effort / priority:** M / P0
- **needs:** privacy-sensitive (message bodies — ships opt-in with explicit
  acknowledgement) · Needs-login (validation — build proceeds from
  documented shapes)

## What it is

Microsoft email — Outlook.com personal, Hotmail, and Microsoft 365
work/school accounts — via the Graph API. Microsoft accounts are
near-universal; alongside Gmail this closes most of the email population.
EWS is fully dead October 2026, so Graph is the only API path and the timing
is exactly right.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Messages (headers + body) | personal + work/school accounts | subject, from/to/cc, ts, body, folder | official Graph docs |
| Raw MIME | same | full RFC822 via `/$value` (fidelity option) | official Graph docs |
| Folders/labels | same | mailFolder names → `labels` | official Graph docs |
| Incremental sync | same | deltaLink cursor | official Graph docs |

All optional in the contract; work-tenant policy may deny consent to a
personal app — the def surfaces the error honestly, no special code path.

## Access & auth

- Graph v1.0: `GET /me/messages`, `/me/mailFolders`, `/me/messages/{id}/$value`.
  OAuth 2.0 scope `Mail.Read`; PKCE for the public client. deltaLink gives
  the standard incremental fetch.
- Connection is registered once as `microsoft` so later defs (Teams chat,
  To Do, OneNote, OneDrive) share it — record the account identity the way
  Google does (by stable subject claim, multi-account capable).
- No TCC. Standalone-clean (plain HTTPS). Bring-your-own client_id fallback
  satisfies the built-for-anyone rule if the compiled-in registration is
  ever throttled or tenant-blocked.

## Vault mapping

- **Raw layer:** optional `correspondence/outlook/raw/` if the `/$value` MIME
  fidelity option is enabled (collection-depth toggle: lean default, raw
  opt-in — matches the configurable-depth convention).
- **Contract layer:** `correspondence/outlook/YYYY-MM.jsonl` per the ratified
  correspondence contract — `guid` = RFC822 Message-ID (dedupes against mbox
  imports of the same mailbox), `chat` = thread subject, `service` = the
  account address, folders in `labels`, Graph immutable id in `extra` for
  cursor work.
- **Dedupe:** Message-ID `guid`; deltaLink cursor in
  `.trove/outlook-sync.json`, rebuildable by re-walking from a full delta.

## Build plan

1. Module `crates/trove-core/src/outlook.rs` (or `sync/outlook.rs` following
   `sync/ticktick.rs`): `DEF` (Periodic), `CONNECTION` (`microsoft`, OAuth),
   `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; later Microsoft
   defs set `connection: Some("microsoft")` — design the token store
   multi-def from day one (mirror the Google module).
3. Fixtures from Graph docs example responses (message list, delta page,
   raw MIME); parser + store + delta-cursor tests, unique temp dirs.
4. Mojibake/HTML-body handling via the existing `mail-parser` path where the
   MIME option is used; Graph JSON bodies otherwise.

## Build status — 🧪 2026-06-14

Shipped (`outlook.rs`, INDEX #10 — a *later* collector in the already-bound
`correspondence` domain, so no binding). `Behavior::Periodic` (15 min).
**"gmail.rs over Microsoft Graph."**

Auth (`CONNECTION` = `microsoft`, OAuth, **multi-account**, shared for future
Teams/ToDo/OneNote/OneDrive): Microsoft Entra **public client + PKCE** (no client
secret), `common/oauth2/v2.0` endpoints, scopes
`offline_access Mail.Read User.Read`, redirect port **38577**, client_id from
`TROVE_MICROSOFT_CLIENT_ID` → empty baked default (BYO). Per-account token stored
0600 under `.trove/sync/microsoft/{id}.json` keyed by the Graph `/me` id
(path-guarded), `offline_access` refresh on expiry (mirrors `sync/google.rs`);
tokens never touch the non-secret cursor/logs.

Pull: Graph `/me/messages/delta` (drain `@odata.nextLink`, persist
`@odata.deltaLink` per account in `.trove/outlook-sync.json`) → per message
`/me/messages/{id}/$value` (MIME) → `email::email_to_message` →
**`correspondence/email/YYYY-MM.jsonl`** (the SHARED email sink, source="email")
with `guid` = Message-ID, `service` = the account address, `labels` = [folder].
401 → refresh, 429 → Retry-After, per-account soft-fail. Opt-in (default-off,
bodies acknowledgement).

**Vault note (deviation from this brief's older text):** writes the shared
`correspondence/email/` sink, NOT `correspondence/outlook/` — so the same mailbox
reached via Outlook + IMAP + an mbox export dedupes on Message-ID (gmail/imap
precedent). The optional raw-MIME `.eml` depth toggle is **deferred** (like
gmail/imap).

Adversarial-verify: 0 blocking + 3 minor — fixed: an expired deltaLink (HTTP 410
Gone) now RESETS the cursor to a full re-delta (was a permanent stall);
already-seen Message-IDs skip the MIME download (dedup before fetch via the delta
stub); `get_mime` honors 429 Retry-After (shared retry helper).

Gate: trove-core 507/0 (+20 outlook tests; serial run — two pre-existing
loopback/lock tests flake under parallelism), `cargo check` clean (our code; a
transitive `imap-proto` future-incompat note is unrelated), `schedule_doc`
regenerated (outlook Periodic), `bindings.ts` up to date. **Live Graph →
Needs-login**; the Azure app registration → **Needs-David**.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Personal account (outlook.com) | 🧪 (Needs-David app + Needs-login) | register the Entra app (below), OAuth a real personal account; Sync now; confirm rows in `correspondence/email/` (service = the account) + hub last-data; second sync is delta-only |
| Work/school account | 🧪 (Needs-login) | needs a Microsoft 365 work account (any real user's run can validate; tenant consent policy may block — confirm the error surfaces honestly) |
| Raw MIME option | 🚫 deferred | depth toggle not built in v1 (lean default; raw `.eml` is a future collection-depth opt-in, like gmail/imap) |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Microsoft Outlook /
Graph (L297-303, 🟢 high) + §Outlook for Mac local OLM store (L305-311,
🟠 low). The OLM/.olk15 local-store mechanism is **rejected**: proprietary,
no public spec, no reliable Rust parser — and Graph covers the same data for
any live account. Users with offline-only .olm archives convert to .mbox and
use the existing email importer; the brief records this so the icebox is
deliberate, not forgotten. EWS deprecation (Oct 2026) makes Graph the only
path going forward. PSTs are Windows-only; Outlook for Mac never produces
them.
