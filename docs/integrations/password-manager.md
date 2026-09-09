# Password Manager Metadata (1Password, Bitwarden)

- **id:** `password-manager`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (user-initiated export, unlocked outside Trove)
- **connection:** none — the user exports from their password manager with
  their master password; Trove never touches the vault or its credentials.
- **evidence:** official-docs — 1PUX format documented at
  support.1password.com/1pux-format/; Bitwarden JSON schema in their public
  docs. Dashlane/LastPass CSVs are similar.
- **effort / priority:** S / P2
- **needs:** privacy (account/service inventory is sensitive even without
  secrets — opt-in with explicit acknowledgement) · Needs-David (review the
  secret-stripping parser before ship)

## What it is

Password managers hold the canonical inventory of every service and account
a person has: item names, categories, URLs, tags, notes, creation/modified
dates. For Trove the value is **metadata only** — "what accounts exist and
when were they created" — which maps a person's digital footprint over
time. Secrets are explicitly, permanently out of scope.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| 1Password .1pux import | all plans | item title, category, tags, URLs, vault name, created/modified dates | official 1PUX docs |
| Bitwarden JSON/CSV import | all plans | name, username, URIs, folders, notes, dates | official Bitwarden docs |
| Other managers (Dashlane, LastPass CSV) | all plans | similar metadata columns | research doc (similar exports) |

All optional in the record shape; a CSV with fewer columns simply yields
sparser rows. No tier-specific code paths.

## Access & auth

- 1Password: File > Export → `.1pux` (a ZIP containing `export.data`,
  nested JSON: account > vaults > items) or CSV.
- Bitwarden: Settings > Export Vault → CSV or JSON (decrypted).
- No API, no connection, no TCC — pure file drop through the generic
  registry import box. The export is unlocked by the user's master password
  *outside* Trove; Trove only ever sees the decrypted export file.
- Standalone-clean: no network, no external process.

## Vault mapping

- **Raw layer:** `files/password-manager/items.jsonl` — one row per vault
  item, **after** secret-stripping (see build plan). Fields: `guid` (item
  UUID from the export), `title`, `category`, `tags[]`, `urls[]`,
  `vault`, `username`, `created_ts`, `modified_ts`, source format in
  `extra`. Notes fields are also dropped by default (they routinely contain
  recovery codes and PINs).
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** item UUID as `guid`; re-import is idempotent.

## Build plan

1. **HARD RULE (the load-bearing step): strip all secret fields at parse
   time** — passwords, TOTP seeds, card numbers, SSNs, security-question
   answers, private keys, recovery codes, and free-text notes. Implement as
   an allowlist (copy only known-safe fields), never a denylist; anything
   unrecognized is dropped. Secrets must never be written to disk, logs, or
   error messages — parse in memory, write only the stripped rows.
2. Module `crates/trove-core/src/password_manager.rs`: `DEF`
   (Behavior::Import), parsers for 1PUX (ZIP + nested JSON) and Bitwarden
   JSON/CSV; format sniffing on drop.
3. Registration line in `INTEGRATIONS`; the generic import box handles UI.
4. Fixtures: hand-built 1PUX and Bitwarden samples from the published
   format docs containing decoy "secrets"; tests assert the decoys appear
   nowhere in the output tree (grep the whole temp vault).
5. Privacy gate: opt-in import copy states what is kept and what is
   stripped. David reviews the stripping parser before ship (needs flag).
6. Do **not** conflate with macOS Keychain (separate source, Security
   framework, not catalogued here).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| 1PUX import | — | export a real 1Password vault; drop the .1pux; confirm item rows in `files/password-manager/` and zero credential strings anywhere in the vault |
| Bitwarden import | — | export Bitwarden JSON; drop; same checks; re-drop and confirm no duplicate rows |
| Secret-strip audit | — | David reviews parser allowlist + fixture grep tests before promote |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Password manager metadata (L2979–L2985) + cross-cutting note 6 (L3007):
the metadata-only rule is a hard requirement of the importer contract.
Feasibility 🟢 high — both formats officially documented. Research doc's
framing ("Security / Metadata") predates the taxonomy; the taxonomy routes
this to `files/`. Not time-sensitive — exports can be re-taken anytime.
