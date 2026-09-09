# Calls & FaceTime

- **id:** `calls`
- **domains:** `correspondence` (contract: ✅ ratified — call rows ride the
  unified stream as `kind:"call"`)
- **status:** 🧪 built (shipped pre-pipeline as `calls.rs`; research notes
  it real-data validated — David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** Periodic (LocalSync; reads CallHistoryDB every ~15 min on
  the shared sync cadence)
- **connection:** none
- **evidence:** community-schema — CallHistory.storedata `ZCALLRECORD`
  layout, already reversed and shipped in
  `crates/trove-core/src/calls.rs` (confidence: proven in production)
- **effort / priority:** S / P0
- **needs:** none

## What it is

The Mac's local call-history database: iPhone calls (synced via
Continuity), FaceTime video and audio, and Mac cellular-relay calls.
Multi-year retention observed — the first sync backfills the full
retained log. macOS Tahoe's native Phone app syncs into the same DB, so
coverage improves for free.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Call log | none (OS feature) | ts, duration, number/handle, direction, answered, service, spam flag | shipped code |

No voicemail — that's a separate provider (`apple-voicemail`, its own
brief). No transcript or audio exists in this DB.

## Access & auth

- `~/Library/Application Support/CallHistoryDB/CallHistory.storedata` —
  Core Data SQLite, `ZCALLRECORD` table: `ZDATE` (Apple-epoch float),
  `ZDURATION`, `ZADDRESS`, `ZSERVICE_PROVIDER`, `ZORIGINATED`,
  `ZANSWERED`, `ZSPAM`.
- Permissions: Full Disk Access (same binary grant as Messages — one FDA
  grant covers both; troved holds it via the signed-build flow).
- Fully local read; no network, no connection. Standalone-clean.

## Vault mapping

- **Raw layer:** none separate — the OS database remains the source;
  rows are normalized straight into the contract (full-fidelity fields
  preserved, e.g. spam flag).
- **Contract layer:** `correspondence/calls/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"call"`, `duration_secs`
  (0 = missed), `chat`/`sender` = the remote handle, `service` =
  Phone/FaceTime variant. Calls share the timeline without counting as
  message volume (read-time semantic in the contract spec).
- **Dedupe:** record id as `guid`; `rowid` drives cursor rebuilds.

## Build plan

Already shipped — `crates/trove-core/src/calls.rs` (`DEF` registered in
`INTEGRATIONS`; Periodic on the shared browser-sync cadence; FDA gate
surfaced via the permission hook). Remaining pipeline work: none for this
provider. The known adjacent gap (voicemail) is deliberately a separate
provider, not an extension here.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Call log | 🧪 shipped pre-pipeline; research doc marks it "built and validated" — David promotes to ✅ | on a Mac with FDA granted: Sync now; confirm recent calls in `correspondence/calls/` and the hub last-data timestamp; place a FaceTime call and see it appear next sync |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Calls
& FaceTime (CallHistoryDB) (L490-L497). Feasibility 🟢 high — the
reference example of a LocalSync collector. Cross-cutting note 5: lives
on troved's slow tick under the existing FDA grant. WhatsApp calls were
investigated and are systematically unreachable (separate `whatsapp`
brief carries that story); Google Voice users get their call log via the
`google-voice` Takeout importer.
