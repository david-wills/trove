# ProtonMail

- **id:** `protonmail`
- **domains:** `correspondence/` (contract: **correspondence — ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user runs Proton's official export tool, drops the
  output folder on Trove)
- **connection:** none — Proton credentials + 2FA are entered into Proton's
  *own* open-source export tool, never into Trove
- **evidence:** official open-source tool —
  github.com/ProtonMail/proton-mail-export (macOS CLI/GUI; emits standard
  .eml + metadata.json; works on free plans) — research feasibility 🟢 high
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (message bodies — explicit opt-in
  acknowledgement at import time)

## What it is

Proton's encrypted email service — disproportionately popular with exactly
the privacy-conscious audience Trove targets. Mail lives encrypted on
Proton's servers; the official export tool decrypts locally and writes
standard EML files, which makes ProtonMail history importable without Trove
ever holding Proton credentials.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full mailbox as .eml files | all plans incl. free | headers, body, attachments-metadata | official tool README |
| Labels/folders | all plans | per-message metadata.json (labels, folder) | official tool output spec |

All optional in the contract; a run without metadata.json still imports
cleanly (labels simply omitted).

## Access & auth

- User runs `proton-mail-export` (official, open-source, macOS build) with
  their Proton login + 2FA → gets a folder of one `.eml` per message plus a
  `metadata.json` per message with label/folder info.
- Trove side: zero auth, zero network, zero TCC. Pure folder import.
- **Rejected path:** Proton Mail Bridge (localhost IMAP at 127.0.0.1:1143)
  needs a *paid* plan and a separately running app — violates the
  standalone rule. Hydroxide (open-source Go bridge) Rust-port/FFI is a
  possible future compiled-in live path; spike-only, not v1.

## Vault mapping

- **Raw layer:** none needed beyond the contract rows — .eml is already the
  full-fidelity artifact and stays in the user's export folder; Trove does
  not copy message files into the vault.
- **Contract layer:** `correspondence/protonmail/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"message"`, `chat` = thread
  subject, `subject`, `to[]`, `service` = the Proton account address,
  `labels[]` from metadata.json, attachments as metadata only.
- **Dedupe:** RFC-822 Message-ID as `guid` — identical to the shipped
  `email` importer; re-imports and overlapping exports skip cleanly.

## Build plan

1. Extend the shipped email importer (`email.rs`, mail-parser crate) to
   accept a **folder of .eml files** alongside a single .mbox — one change
   that serves ProtonMail and any other EML-emitting tool.
2. Module `crates/trove-core/src/protonmail.rs`: `DEF` (Import) that
   delegates to the shared eml parser, writes `source:"protonmail"`, and
   layers in `metadata.json` labels when present. Setup copy on the def
   points users at the official export tool.
3. One registration line in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: a small export-tool output folder (eml + metadata.json,
   plus a metadata-missing variant); parser/dedupe/store tests in unique
   temp dirs.
5. Import box shows the privacy acknowledgement (full message bodies enter
   the vault) before the first run.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| EML folder import | ✅ unit-tested | run proton-mail-export on a real (free) Proton account; drop the folder on the import box; confirm rows in `correspondence/protonmail/`, hub last-data, dedupe on second import |
| Labels via metadata.json | ✅ unit-tested | same run; confirm `labels[]` on rows matches Proton folder names from labels.json |
| Single .eml file import | ✅ unit-tested | drop any single .eml from the export on the import box |
| No-sidecar fallback | ✅ unit-tested | confirms guid/body still imported from EML headers when metadata.json absent |

## Build notes (2026-06-16)

- Implemented as `Import` behavior (no network, no credentials); behaviour confirmed via 7 passing unit tests.
- EML parsing reuses `email::email_to_message`; writes to `correspondence/protonmail/` via the shared `append_messages` sink.
- Metadata sidecar (`{ID}.metadata.json`) carries `ExternalID` (RFC-822 Message-ID → guid) and `LabelIDs` (resolved to names via `labels.json`).
- Handles both array and object forms of `labels.json` (both seen in source test data).
- Re-import dedupes via the `correspondence_guids("protonmail")` set (same scheme as gmail/email/fastmail).
- `accepts: &["eml"]` — user can drop the folder path or a single .eml; the run fn detects `path.is_dir()` and walks all `.eml` files.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §ProtonMail
(L313–L319) + §ProtonMail native export tool (L433–L439). Feasibility 🟢
high. The export tool is separate from Bridge and free-plan-friendly. The
cross-cutting unified-correspondence-sink note (L443) applies: this is the
same sink + Message-ID guid scheme email.rs already uses. Not
time-sensitive — Proton retains mail; export any time.
