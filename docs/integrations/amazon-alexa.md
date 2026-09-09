# Amazon Alexa

- **id:** `amazon-alexa`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT shape; voice
  command history is the Alexa interaction stream)
- **status:** 🧪 built (raw scaffold + parked parser; home.event contract deferred)
- **unavailable_reason:** none
- **behavior:** Import (user-supplied Amazon privacy data export; no live API
  for ongoing pull)
- **connection:** none — the user requests the export from Amazon's privacy
  portal themselves; Trove never logs in.
- **evidence:** community-schema — amazon.com/privacy data export;
  `alexa/voice_history.json` format (medium confidence) · sample-required for
  the exact export JSON shape
- **effort / priority:** S / P2
- **needs:** privacy (voice command transcripts are message-body-equivalent
  content — opt-in with explicit acknowledgement) · Needs-sample (the export
  JSON layout is community-described, not officially documented) ·
  time-sensitive (export covers ~18 months — import periodically or lose
  older data)

## What it is

Amazon Alexa logs every voice interaction with Echo/Alexa devices: the
timestamp, the device, the spoken command transcription, and Alexa's response
text. For Trove it's a voice-query activity log under `home/` — a record of
how and when the user interacted with their smart-home assistant. Audio
recordings are excluded from the export; only the transcribed text is
available. Anyone with an Amazon/Alexa account can request this; it's a
meaningful slice of daily home interaction otherwise uncaptured.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Voice interaction history | all accounts (export) | timestamp, device name, command text, Alexa response text | community (`voice_history.json`) |
| Smart-home device events | uncertain — may be in `smart_home_history.json` | device event log (NOT reliably included) | community (verify per portal) |

All optional in the contract (omit-if-empty). Audio is never included —
transcripts only.

## Access & auth

- **No API.** Manual export only: amazon.com/privacy → Request My Data →
  "Alexa interaction history". The export arrives via an emailed link in
  24–72 hours. File: `alexa/voice_history.json` (prefer JSON over CSV for
  metadata fidelity). `alexa/smart_home_history.json` may or may not be
  present — categories vary; check the portal.
- Auth: none in Trove — the user downloads the file and hands it to the
  importer. Standalone-clean (no login, no scraping, no runtime dependency).
- No TCC; the user picks the local file in the import box.
- **Coverage caveat:** export spans up to ~18 months; older interactions are
  gone. The Alexa Skills API is for skill developers, not personal-data
  retrieval — there is no programmatic ongoing pull. (As of March 2025 Amazon
  routes all Alexa+ interactions to the cloud; this does not change the export
  path, which remains transcripts-only.)

## Vault mapping

- **Raw layer:** `home/amazon-alexa/raw/` — the export file(s) parsed to JSONL
  at full fidelity, partitioned by month of the interaction.
- **Contract layer:** `home/amazon-alexa/YYYY-MM.jsonl` per the (pending)
  home contract — expected one row per voice interaction (`ts`, `source`,
  `guid` = interaction id or timestamp+device hash, `device`, `command`,
  `response`), overflow in `extra`. The voice command text is conversation
  content — it stays whole on the row but is gated (see privacy).
- **Dedupe:** interaction id (or a stable timestamp+device hash) as `guid`;
  re-importing a fresh export is idempotent (upsert by guid), which is how the
  rolling 18-month window accumulates across imports.

## Build plan

1. Module `crates/trove-core/src/amazon_alexa.rs`: `DEF` (Import; import-box
   copy explaining the amazon.com/privacy request + 24–72h turnaround + the
   18-month window), parse + store hooks. No connection (no auth).
2. Registration line in `INTEGRATIONS`.
3. **Parser-last / Needs-sample:** the `voice_history.json` layout is
   community-described — build the parser against a real export sample before
   wiring it on; probe whether `smart_home_history.json` is present and parse
   it when available.
4. Privacy gate: ships opt-in (voice command transcripts ≈ message bodies) —
   explicit acknowledgement on enable.
5. Time-sensitivity: surface a UI hint that the export only covers ~18 months,
   so users import periodically rather than losing older history.
6. Vault writes via `store` helpers once the home contract is ratified; until
   then **parked behind Needs-David (contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Voice interaction history | 🧪 raw-scaffold | drop `voice_history.json` in the import box; confirm verbatim snapshot lands in `home/amazon-alexa/raw/`; hub last-data populated; parser returns parked message with raw-storage confirmation |
| Smart-home events | ⏸ deferred | parked with voice parser; no sample available |

## Build notes (2026-06-16)

- Replaced `NotWired` stub with `Behavior::Import` scaffold.
- Raw layer stores verbatim export snapshot under `home/amazon-alexa/raw/<timestamp>-<filename>`;
  re-imports accumulate (each gets its own dated filename — the 18-month roll means each
  export has new data).
- Parser parked (`interactions_from_export` → bail! with `PARKED_MSG`): the
  `voice_history.json` field names are community-described with no real sample confirmed.
  `PARKED_MSG` names the function to fill when a sample lands.
- Contract layer (home.event) deferred: the home.event shape is an unbound sibling draft
  (`home.event` schema exists at `docs/vault-spec/schemas/home.event.schema.json` but no
  Rust type is bound yet). When unparked: each interaction → one `event:"command"` line with
  `device`, `detail` = utterance, `extra.response` = reply, `guid` = stable id or
  `device|ts`.
- `last_data` uses `newest_mtime` (raw files are `.json` not `.jsonl`).
- 6 tests: raw storage, accumulation, DEF introspection, last_data before/after, parked message.
- `contract_mode`: `deferred-sibling-draft` / `deferred_sibling_draft`: `home.event`
- No new deps, no new ConnectionDef, no new CONNECTIONS line, no contract file touched.

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Amazon Alexa (voice
history export) (L1944–L1950). Feasibility 🟡 medium. Export is manual and
asynchronous (24–72h), audio recordings excluded, smart-home event logs not
reliably included — only voice command transcriptions. No live API exists;
the export is the only path. The ~18-month coverage window makes this
time-sensitive: periodic re-import is required to retain history.
