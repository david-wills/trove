# Mac Microphone Ambient Sound

- **id:** `macos-microphone`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  domain; ambient-readings shape drafted in the contract pass. Same-shaped
  readings merge with `home/`-owned-device and public-feed sources at read
  time.)
- **status:** 📋 queued
- **behavior:** Live (continuous sampler; writes a watermarked stream)
- **connection:** none — local microphone via TCC; no account, no network.
- **evidence:** official-docs — `AVAudioEngine` / `AVAudioInputNode` RMS
  metering on macOS (`averagePower(forChannel:)` / current-level-meter dB);
  TCC Microphone permission. Hardware path is documented and works; absolute
  calibration is the open caveat.
- **effort / priority:** M / P2
- **needs:** **privacy-sensitive** (continuous microphone access — ships
  opt-in, default-off, with explicit acknowledgement) · Needs-David (opt-in
  design + explicit disclosure sign-off before it ships) · environment
  contract not yet ratified

## What it is

Samples the built-in Mac microphone to log an *ambient loudness level* (dB) —
quiet office vs. noisy café vs. loud street — as a slow time series. It never
records or stores audio content: only an RMS-derived dB number per window.
Useful as ambient context for the day timeline. Best-effort: a laptop mic is
positioned and gained inconsistently across machines, so values are relative
trends, not calibrated SPL.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Ambient dB level | n/a (local hardware) | RMS dB per sampled window, `ts` | AVAudioEngine metering (official) |
| Loud-environment flag | n/a | derived boolean above a threshold (optional) | derived |

All optional in the contract (omit-if-empty). No audio, no transcript, no
frequency content is ever written — that constraint is a hard parse-time rule,
not a config option.

## Access & auth

- `AVAudioEngine` taps `AVAudioInputNode`; compute RMS power over a short
  window and convert to dB. Sample sparsely — a 5-second RMS window every
  ~5 minutes is the research recommendation, never a continuous open stream
  beyond the metering window.
- **TCC:** Microphone permission (System Settings → Privacy & Security →
  Microphone). This is a heavyweight ask — users are rightly wary of mic
  access — so the card must explain exactly what is and isn't captured before
  the prompt.
- No network, no account, standalone-clean. Spike first to confirm metering
  works headless and the dB values are stable enough to be useful.

## Vault mapping

- **Raw layer:** none beyond the contract stream — there is no richer native
  shape to preserve (we deliberately discard audio).
- **Contract layer:** `environment/macos-microphone/YYYY-MM.jsonl` per the
  (pending) environment ambient-readings contract — expected one row per
  sampled window (`ts`, `source`, the dB reading, optional loud flag), device
  identifier in `extra`. Parked behind the contract draft (Needs-David) and
  the privacy sign-off.
- **Dedupe:** sample timestamp is the natural key; watermark cursor in
  `.trove/macos-microphone-sync.json`, rebuildable by scanning output files.

## Build plan

1. **Spike first:** prove `AVAudioEngine` RMS metering runs headless under TCC
   and yields a usable dB series; confirm fan/keyboard noise doesn't swamp it.
2. Module `crates/trove-core/src/macos_microphone.rs`: `DEF` (Live),
   `permission` hook reporting TCC Microphone status. No `CONNECTION`.
3. Registration line in `INTEGRATIONS`.
4. **Privacy gate:** default-off, opt-in with an explicit disclosure card
   ("samples ambient loudness only; never records or stores audio"). David
   signs off on the disclosure copy before it ships — this is the
   mandatory privacy-sensitive flag.
5. Hard rule in the sampler: store dB only; the raw audio buffer is metered
   and dropped in-frame, never persisted.
6. Vault writes via `store` helpers once the environment contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Ambient dB level | — | enable opt-in, grant Microphone TCC; let it sample; confirm dB rows in `environment/macos-microphone/` + hub last-data; verify quiet vs. loud rooms differ |
| No-audio guarantee | — | inspect vault + memory: confirm only dB numbers are written, no audio buffers persisted anywhere |
| TCC-denied state | — | deny Microphone permission; confirm a clean disabled state with the affordance hint, no error spam |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §macOS Microphone
— Live Ambient dB Level (L2160–L2167). Feasibility 🟡 medium: API works, but
consumer mics aren't calibrated, so values are relative-trend only, not
cross-device-comparable SPL. The Apple Watch environmental-audio-exposure
path (via the Health export, `HKQuantityTypeIdentifierEnvironmentalAudioExposure`)
is a *better* noise source for Watch owners and adds no new integration —
offer this Mac-mic stream as the no-Watch fallback. TCC mic access is a
significant trust ask; opt-in + explicit disclosure is non-negotiable.
