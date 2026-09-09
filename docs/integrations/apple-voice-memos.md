# Apple Voice Memos

- **id:** `apple-voice-memos`
- **domains:** `voice` (contract: **not yet ratified** — Phase 3 decides
  whether Voice Memos / Visual Voicemail / Google Voice voicemails converge
  on one shape or stay raw-only)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `voice`; CloudRecordings.db + tsrp transcript; needs FDA + real memos to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (troved slow tick; scan the local Recordings DB for
  new rows)
- **connection:** none (local files; FDA-gated path)
- **evidence:** community-schema, high confidence — path + DB schema
  confirmed, `tsrp` atom parsing has reference implementations
  (github.com/jwulff/apple-voice-memo-mcp, pedramamini's voice-memos gist)
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (personal audio recordings + transcripts —
  opt-in with explicit acknowledgement) · Needs-David (contract: voice)

## What it is

Apple's built-in recorder. iCloud sync means iPhone memos land in the same
Mac-local directory — so this one collector captures every voice note the
user has ever taken, on any device. On macOS 15+ Apple embeds its own
on-device transcript inside the audio file, making this a zero-dependency
transcript source.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Memo metadata | all | title, date, duration, file path | community-schema (CloudRecordings.db) |
| Native transcript | recordings made on macOS 15+/recent iOS | full transcript text | community-schema (`tsrp` atom, reference parsers) |
| Whisper transcript (enrichment) | opt-in, pre-Sequoia recordings | transcript text | research recommendation (bundled whisper.rs/candle — later pass) |

All optional in the contract shape; older memos simply carry no transcript
until the optional Whisper pass runs.

## Access & auth

- Local DB: `~/Library/Group Containers/group.com.apple.VoiceMemos.shared/
  Recordings/CloudRecordings.db` (SQLite); audio as
  `YYYYMMDD HHMMSS[-hash].m4a`/`.qta` in the same directory.
- Native transcripts: `tsrp` UDTA atom inside the `.m4a` — JSON
  (`{"attributedString":…}`) extractable by a ~50-line binary scan. No ML,
  no external tools.
- Permission: Full Disk Access (Group Containers path is TCC-gated) —
  **already held by troved**; reuse the existing FDA gate pattern from
  `imessage.rs`, never re-prompt.
- Standalone-clean: zero network, zero external apps. Optional Whisper
  enrichment must be a *bundled* model (whisper.rs/candle), never an Ollama
  dependency.

## Vault mapping

- **Raw layer:** `voice/apple-voice-memos/YYYY-MM.jsonl` — one row per memo:
  `ts`, `guid` (DB row UUID), `title`, `duration_secs`, `transcript`,
  `file` (path to the user's original audio; no audio copies into the
  vault). *(Research doc said `voice-memos/`; the taxonomy table wins:
  `voice/` domain, per-source folder.)*
- **Contract layer:** pending the Phase 3 voice decision; if voice stays
  raw-only, the raw layer above is the final shape and just needs its
  conventions documented.
- **Dedupe:** recording UUID as `guid`; watermark on DB date column in
  `.trove/apple-voice-memos-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/apple_voice_memos.rs` (def id
   `apple-voice-memos`): `DEF` (Periodic), permission hook = FDA check,
   last-data + pull hooks. No connection.
2. Registration line in `INTEGRATIONS`.
3. `tsrp` atom parser: binary scan for the atom + JSON extraction, ported
   from the jwulff/pedramamini reference logic. Prioritize this over any
   Whisper work — it covers all modern recordings for free.
4. Fixtures: a small synthetic CloudRecordings.db + an `.m4a` with a known
   `tsrp` atom; tests for atom-present, atom-absent (pre-Sequoia), and
   `.qta` files; unique temp dirs.
5. Opt-in gate: default-off toggle with explicit copy about reading personal
   recordings and their transcripts.
6. Whisper enrichment for pre-15 audio is a separate later pass, opt-in,
   bundled model only — not in this iteration.

## Build status — 🧪 2026-06-14

Shipped (`apple_voice_memos.rs`, INDEX #17 — also the **first-in-domain binding**
of the `voice` contract). `Behavior::Periodic`, FDA-gated (imessage permission
pattern), **default-off 🔒** (personal recordings + transcripts). No connection.

Reads `~/Library/Group Containers/group.com.apple.VoiceMemos.shared/Recordings/
CloudRecordings.db` (SQLite, copy-then-open via rusqlite; `TROVE_HOME` override for
tests) — **schema-adaptive SELECT** (`PRAGMA table_info`) for OS-version
robustness. Per recording → `Recording` (kind="memo"): `ts` = the Core Data date
(**ZDATE + 978307200** → local), `title`, `duration_secs`, `guid` =
`ZUNIQUEID`(UUID) → ZPATH-stem → `Z_PK`, `audio_ref` = the user's **original `.m4a`
path (no copies into the vault)**, `transcript` from the `tsrp` atom. Output
`voice/apple-voice-memos/YYYY-MM.jsonl` (month of `ts`), dedup/upsert by `guid`,
watermark cursor `.trove/apple-voice-memos-sync.json`. FDA-unreadable → graceful
no-op (available:false).

Native transcript: a binary scan of the `.m4a` for the `tsrp` UDTA atom →
`{"attributedString":{"runs":[str,idx,…]}}` → concatenate the string runs (macOS
15+/recent iOS embed it). Atom-absent (pre-15) / malformed / `.qta` → omit
transcript gracefully. (Whisper transcription for pre-15 audio is a deferred
opt-in, bundled-model-only.)

Contract binding (first collector in `voice/`): added the `Recording` struct + the
`voice` `DOMAINS` entry (EventStream, month of `ts`, required `ts`/`source`/`kind`),
promoted the fixtures from the draft test to the ratified triad (5/5). The
carry-forward (`from`→`sender`, confidence→`extra`) was already in the schema.

Evidence: the `CloudRecordings.db` schema + the `tsrp` atom layout were verified
against the `jwulff/apple-voice-memo-mcp` + pedramamini + Countz reference parsers
(private format, no official docs).

Adversarial-verify: **NO DEFECTS FOUND** (binding triad, Core Data epoch, tsrp
parse incl. `.qta`/multi-run/unicode/edge-cases, guid/dedup/watermark,
no-audio-copy, FDA-degradation + panic-safety all confirmed).

Gate: trove-core 582/0 (+7 voice-memos tests), `cargo check` clean, `schedule_doc`
regenerated (apple-voice-memos Periodic), `bindings.ts` up to date.

**Contract note (for David):** `voice.md` describes `audio_ref` as "vault-relative,"
but the no-copies rule writes the user's original absolute path — suggest softening
the contract text to "vault-relative OR original absolute path for local no-copy
sources."

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Metadata pull | 🧪 (Needs-David: FDA) | grant FDA to troved (shared with imessage/books); enable the toggle on a Mac with existing memos; Sync now; confirm rows in `voice/apple-voice-memos/` + hub last-data |
| Native transcript | 🧪 (Needs-David: FDA) | record a memo on macOS 15+ (or synced from a recent iPhone); confirm the row carries the `transcript` text (from the `tsrp` atom) |
| iCloud-synced iPhone memos | 🧪 (Needs-David: FDA) | record on iPhone, wait for sync, pull; confirm it appears |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Apple
Voice Memos (L530-537; at-a-glance L473; cross-cutting notes 5 and 7).
Feasibility 🟢 high; was already "planned" pre-pipeline. The `tsrp` atom is
the key research finding — zero-dependency transcripts on macOS 15+. Not
time-sensitive (recordings persist locally/iCloud).
