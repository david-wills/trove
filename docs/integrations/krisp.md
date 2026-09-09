# Krisp

- **id:** `krisp`
- **domains:** `meetings` (contract: **not yet ratified** — Phase 3 drafts it
  from Granola + Fathom + Zoom + Fireflies together; Krisp conforms once
  ratified)
- **status:** 🧪 built (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (manual `.txt` transcript export from the Krisp
  dashboard — the only confirmed local-first path; see spike note below)
- **connection:** none (import needs no login; a future MCP/local path may
  change this)
- **evidence:** community-schema — webhook + MCP mentioned in Krisp 3.10.5
  release notes; MCP locality **unverified**; `.txt` export format
  undocumented → sample-required
- **effort / priority:** M / P2
- **needs:** Privacy-sensitive (opt-in) · Needs-sample (.txt export) ·
  Needs-David (contract: meetings)

## What it is

Krisp is primarily a system-wide noise-cancellation tool that added meeting
notetaking (transcripts, notes, outlines) as a secondary feature. Because it
sits on the audio device it captures meetings across Zoom, Teams, Meet, etc.
without a bot participant. Its programmatic surface is webhook-push — which
needs a public HTTPS endpoint and so violates Trove's local-first/standalone
rule — leaving manual export as the honest v1.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transcript export (.txt) | account (tiering undocumented) | transcript text | release notes / dashboard, no format docs |
| Notes/outline JSON | webhook only (not local-first) | transcript, notes, outline | release notes |
| Local MCP data | unknown — spike | unknown | release-notes mention only |

All optional in the contract; the import path yields transcript text and
whatever metadata the export embeds (unknown until a sample exists).

## Access & auth

- **Import (v1):** user downloads `.txt` transcripts from the Krisp
  dashboard and drops them in the registry-driven import box. No auth, no
  TCC.
- **Webhook (rejected):** Krisp POSTs JSON to a configured URL on meeting
  completion — requires an internet-reachable endpoint; not local-first.
- **MCP (spike):** official Krisp MCP exists per 3.10.5 release notes; if it
  runs locally off the app's data it becomes an M3/M6 path with no
  networking. Locality is unverified from public docs.

## Vault mapping

- **Raw layer:** `meetings/krisp/raw/` — imported transcript files kept
  verbatim (artifact files, not JSONL, until the format is known).
- **Contract layer:** `meetings/krisp/YYYY-MM.jsonl` per the (pending)
  meetings contract — one row per imported meeting; `guid` from a content
  hash + meeting date until the export proves a stable id; transcript as
  sidecar document.
- **Dedupe:** content-hash `guid` (re-importing the same file is a no-op).

## Build plan

1. **Parser-last (Needs-sample):** the `.txt` export format is undocumented —
   do not write the parser from folklore. Acquire a real export first; the
   loop iteration stalls here without one.
2. Spike (timeboxed, during the build slot): determine whether the Krisp MCP
   server is local-socket or cloud-relay. Local → upgrade path to automatic
   collection; cloud → stay Import.
3. Module `crates/trove-core/src/krisp.rs`: `DEF` (Import; the registry
   import box handles file intake), one line in `INTEGRATIONS`.
4. Fixtures from the acquired sample; parser + store tests, unique temp dirs.
5. Privacy: transcripts are message-body-class — default-off, explicit
   opt-in copy on the card.
6. Parked behind Needs-David (meetings contract) like the rest of the domain.

## Build notes (2026-06-16)

Module built as `Behavior::Import` (`.txt`), reusing the `meetings` contract.
Parser is **parked** — the `.txt` format is undocumented and no sample exists.
The scaffold stores every imported file verbatim under `meetings/krisp/raw/<sha256-hash>.txt`
and writes a minimal contract row (guid = content hash, transcript_ref = raw path).
Structured fields (title, started, attendees, platform) stay empty until a real sample
confirms the structure. 5 unit tests pass; cargo check clean.

**ts uses file mtime as fallback** (not import-day noon): a historical transcript
exported from a March meeting and imported in June lands in the `2024-03.jsonl`
partition, not `2026-06.jsonl`. Falls back to noon-today only if mtime is
unavailable (e.g. piped input). Verified by `ts_uses_file_mtime_not_today` test.

MCP spike not completed — locality (local-socket vs cloud-relay) is unverified from
public docs. If the MCP runs locally, this can be upgraded to Periodic/NativeHost.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| .txt import (scaffold) | ✅ built | `cargo test -p trove-core krisp::` — 5/5 pass |
| .txt format parser | ⏳ parked | acquire a real Krisp export; fill parser in `krisp.rs`; no shared files to touch |
| MCP locality spike | — | inspect the Krisp MCP's transport with the app running; record local vs cloud in this brief |

## Research notes

`integrations-research.md` → "Calls, Voice & Meeting Transcripts" §Krisp
Meeting Notes (L586–L593). Feasibility 🟡 medium — entirely because the
webhook model conflicts with local-first. Time-sensitive flag: webhook
payloads aren't replayable and dashboard retention is unstated, so users
should export periodically. Research recommendation: spike first; manual
export is always the fallback.
