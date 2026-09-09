# IMAP Email (any provider)

- **id:** `imap`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built (fixture-tested; sync IMAP over rustls; single mailbox; needs a real mailbox to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (UID-based incremental fetch per folder)
- **connection:** `imap` — TokenPaste (host + username + app password; the
  common case) with XOAUTH2 as a later method for providers that ban
  passwords. Per-account: a user may add several mailboxes.
- **evidence:** official-docs — RFC 3501 IMAP4rev1 + XOAUTH2 SASL;
  `async-imap` crate (actively maintained, chatmail org)
- **effort / priority:** M / P0
- **needs:** privacy-sensitive (message bodies — opt-in with explicit
  acknowledgement) · Needs-login (validation needs a real mailbox; build
  proceeds from the RFC + fixtures)

## What it is

The universal email protocol: one generic collector serves iCloud Mail,
Yahoo, Zoho, Fastmail (as fallback), self-hosted, and any custom-domain
mailbox — every provider Trove will never write a bespoke integration
for. This is the "built for anyone" workhorse of the correspondence
domain: whatever email a user has, this reaches it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full message history (backfill) | any IMAP mailbox | RFC822 via FETCH BODY.PEEK[] → ts, chat, sender, text, to, subject, attachments meta | RFC 3501 |
| Incremental new mail | any IMAP mailbox | same shape via UIDVALIDITY + last-seen UID per folder | RFC 3501 |
| Folder names as labels | any IMAP mailbox | `labels[]` (mailbox/folder name) | RFC 3501 |

All optional in the contract; no tier gating — IMAP is IMAP.

## Access & auth

- RFC 3501 over port 993 TLS. FETCH `RFC822` / `BODY.PEEK[]` for verbatim
  bytes — same parser as the mbox importer and Gmail pull.
- Auth: app-specific password is the v1 path (iCloud: `imap.mail.me.com`,
  Apple ID → security → app passwords; most providers offer equivalents).
  XOAUTH2 covers Gmail/Outlook but those have dedicated providers — keep
  XOAUTH2 out of v1.
- Rust: `async-imap`. No TCC; standalone-clean (plain TLS socket).
- ProtonMail-via-Bridge is explicitly NOT this collector's job (Bridge
  requires a running app — see the `protonmail` brief's export-tool path).

## Vault mapping

- **Raw layer:** none separate — RFC822 bytes decode straight to contract
  rows (full fidelity is the parsed message itself).
- **Contract layer:** `correspondence/email/YYYY-MM.jsonl` — the shared
  email sink per the ratified correspondence contract, `service` = the
  account address. Message-ID as `guid` keeps IMAP, Gmail pull, mbox
  import, and Apple Mail mutually dedupe-safe (the same mailbox reached
  two ways never duplicates).
- **Cursor:** per-folder UIDVALIDITY + max UID in sync state; rebuildable
  by re-running backfill (guid dedupe absorbs overlap).

## Build plan

1. Module `crates/trove-core/src/imap.rs`: `DEF` (Periodic) + `CONNECTION`
   (TokenPaste: host / username / app-password fields, setup copy walking
   through app-password creation with iCloud as the worked example).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Reuse the shared RFC822→Message parser from `email.rs`/`gmail.rs`;
   add UID-cursor logic + UIDVALIDITY-reset handling (full refetch, guids
   dedupe).
4. Fixtures: canned FETCH responses (plain, multipart, attachment-bearing,
   UIDVALIDITY rollover); parser + cursor + store tests, unique temp dirs.
5. Opt-in acknowledgement (message bodies) on enable, per the
   privacy-sensitive rule.

## Build status — 🧪 2026-06-14

Shipped (`imap.rs`, INDEX #9 — a *later* collector in the already-bound
`correspondence` domain, so no binding). `Behavior::Periodic` (15 min). Sync IMAP
via the `imap` crate (v2.4.1) over **rustls** (pure-Rust `ring` backend — no
OpenSSL/aws-lc, standalone-clean); reuses `email::email_to_message` (the same
RFC822 parser as the Gmail pull + mbox import).

Auth (`CONNECTION` = TokenPaste, single field): paste `email app-password
[host [port]]`. The host is derived from the email domain via a built-in
well-known-provider table (iCloud/Yahoo/Zoho/Fastmail/GMX/…); an unknown domain
with no explicit host returns a clear "append the host" error. Creds are stored
as a **0600 secret** (`save_sync_token` → `.trove/sync/`); the password never
touches the non-secret cursor, logs, or error strings. **v1 = a single mailbox**
(multi-account + XOAUTH2 deferred — single-field TokenPaste can't cleanly
multi-account, and Gmail/Outlook have dedicated providers).

Pull: opens folders **read-only** (`EXAMINE`) and fetches **`BODY.PEEK[]`** — it
never marks mail `\Seen` or mutates the mailbox. Per-folder cursor
(`.trove/imap-sync.json`): on a `UIDVALIDITY` change (or a new folder) it
refetches from UID 1; otherwise it fetches strictly `UID > max_uid`. RFC822
bytes → `email_to_message` → `correspondence/email/YYYY-MM.jsonl` with
`service` = the account address and `labels` = [folder]; dedup by `guid`
(Message-ID), so IMAP / Gmail / mbox / Apple Mail never double a message. Bodies
+ attachment *metadata* only (no raw `.eml` or attachment downloads — a deferred
collection-depth opt-in, like gmail).

Adversarial-verify: 0 blocking + 2 minor — removed a `proton.me`→`127.0.0.1`
host-table trap (Proton-via-Bridge isn't this collector's job); accepted
"cursor advances past a permanently-unparseable top message" as by-design
(matches gmail/mbox; the alternative perpetually re-fetches the tail).

Gate: trove-core 487/0 (+15 imap tests), `cargo check` clean (our code; the
transitive `imap-proto` carries a Rust future-incompat *note* — tracked, not a
current failure), `schedule_doc` regenerated (imap Periodic), `bindings.ts` up to
date. **Live IMAP socket needs a real mailbox → Needs-login** (David validates).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Backfill | 🧪 (Needs-login) | add an iCloud mailbox: paste `you@icloud.com <app-password>` (host auto-derived); Sync now; confirm rows in `correspondence/email/` + hub last-data |
| Incremental | 🧪 (Needs-login) | send self a mail, Sync now, exactly one new row (no dup of prior mail) |
| Second provider | 🧪 (Needs-login) | repeat against a non-Apple host (Zoho/self-hosted, appending the host) to prove genericity |
| Non-mutation | 🧪 (Needs-login) | confirm synced messages are NOT marked read in the mailbox (BODY.PEEK + EXAMINE) |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Generic IMAP
(L217-L223). Feasibility 🟢 high. Catalog note: server-deleted mail is
gone — surface "connect early" in the setup copy. Fastmail gets a
dedicated JMAP provider (strictly better sync); IMAP remains its
fallback. Build alongside/after Gmail so the shared RFC822 parser is
already exercised.
