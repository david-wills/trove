# Signal

- **id:** `signal`
- **domains:** `correspondence/` (contract: **correspondence — ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll Signal Desktop's local DB, like iMessage)
- **connection:** none — local access only: Full Disk Access + a macOS
  Keychain read for Signal's DB encryption key
- **evidence:** community schema — Signal-Desktop GitHub + vmois.dev
  writeups (medium confidence; schema shifts with app updates); key
  location per Signal-Desktop PR #6849 (Keychain via Electron safeStorage)
- **effort / priority:** L / P2
- **needs:** privacy-sensitive (message bodies — explicit opt-in with
  clear explanation of the Keychain access) · time-sensitive-adjacent
  (disappearing messages vanish from the DB; app updates can shift the
  schema under us)
- **caveat:** requires Signal **Desktop** — phone-only history never
  reaches the Mac

## What it is

The privacy-first messenger. By design it has **no export feature and no
API** — the only access path is Signal Desktop's local SQLCipher-encrypted
SQLite DB. High privacy value (these are exactly the conversations users
keep nowhere else), medium-high complexity.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Messages (DMs + groups) | needs Signal Desktop installed/linked | text, sender, timestamp, conversation | community schema (vmois.dev) |
| Reactions / attachments-metadata | same | reaction emoji, attachment names/types | community schema |

All optional in the contract. Disappearing messages yield only what still
exists in the DB at poll time — an honest limitation to state in the UI.

## Access & auth

- DB: SQLCipher-encrypted SQLite at
  `~/Library/Application Support/Signal/sql/db.sqlite` (FDA to read;
  copy-then-read like imessage.rs).
- Key: since mid-2024 (PR #6849) stored in the macOS Keychain under the
  service **"Signal Safe Storage"** via Electron safeStorage; the user must
  approve Trove's Keychain access per-app. Older un-updated installs may
  still have a plaintext key in `config.json` — support both, prefer
  Keychain.
- Decrypt with the sqlcipher Rust crate; tables include `conversations`,
  `messages`. Standalone-clean: no running Signal process required (unlike
  WeChat).
- UX is the hard part: the Keychain prompt is system-owned and scary —
  the connect/permission card must explain *why* Trove needs Signal's key
  before triggering it (disabled-controls-need-affordance rule).
- Rejected fallback: Signal Desktop's per-conversation "Export chat"
  (plaintext .txt) — very lossy, not worth a parser.

## Vault mapping

- **Raw layer:** none beyond contract rows; source-native extras
  (quote/story references, expiry timers) ride in `extra`.
- **Contract layer:** `correspondence/signal/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"message"`/`"reaction"`,
  `chat` = conversation id, `sender` = e164/ACI handle (raw handles;
  contacts mapping is read-time), `from_me`, `text`, `reply_to` for
  reactions, `service:"Signal"`.
- **Dedupe:** Signal message id as `guid`; poll cursor (max rowid/
  received_at) in `.trove/signal-sync.json`, rebuildable from output.

## Build plan

1. Spike first: confirm Keychain item name + safeStorage key derivation
   and current schema against a live Signal Desktop install (schema drift
   is the known risk — pin fixtures to an app version, detect unknown
   schema and fail soft with a clear hub message).
2. Module `crates/trove-core/src/signal.rs`: `DEF` (Periodic, ~15 min like
   iMessage), permission hook covering both FDA and the Keychain grant,
   copy-then-read, sqlcipher open, incremental query.
3. One registration line in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: a synthetic SQLCipher DB built in-test with the documented
   schema (both key paths: Keychain-style key + legacy config.json);
   parser/cursor/dedupe tests, unique temp dirs.
5. Opt-in gate: privacy acknowledgement + Keychain explainer before the
   toggle arms.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Keychain key + decrypt | ✅ unit-tested (round-trip) | real Mac with Signal Desktop linked; enable toggle; approve Keychain prompt; confirm rows in `correspondence/signal/` |
| Legacy config.json key | ✅ stub path in code | needs an un-updated pre-2024 install — rare; validate via fixture only unless one surfaces |
| Schema-drift guard | ✅ fails-soft via anyhow context | bump fixture schema version; confirm collector fails soft with the hub message, not a crash |
| Parser / cursor / vault-write | ✅ 5 unit tests green | `cargo test -p trove-core signal::` |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Signal Desktop
(L329–L335); cross-cutting note 2 (encryption-key barrier, L445): Signal is
the *principled* case — key on disk (Keychain), no running app needed —
unlike WeChat (memory-only key, hard-blocked). Feasibility 🟡 medium.
Build-later priority stands: high privacy value, but Keychain UX + schema
drift make it the most fragile correspondence collector; sequence after
the export-based messengers prove the sink.
