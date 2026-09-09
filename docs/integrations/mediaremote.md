# Mac Now Playing (MediaRemote)

- **id:** `mediaremote`
- **domains:** `media/plays/mediaremote/` (contract: **media-plays,
  ratified**)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Live (subscribe to the Mac's system now-playing stream;
  emit play rows as tracks/videos change)
- **connection:** none
- **evidence:** community — mediaremote-rs / mediaremote-adapter crates
  (Perl-adapter workaround, JSON over stdout); BetterTouchTool community
  confirms both the macOS 15.4 breakage and that the /usr/bin/perl
  workaround resolves it. No official docs — private framework.
- **effort / priority:** M / P1
- **needs:** Needs-sample + **Needs-David** (spike: verify the Perl adapter's
  output on **current macOS (26.5.1 here)** before committing — the workaround is
  the build's foundation, it circumvents an active Apple restriction, and Apple
  can move it). **PARKED 2026-06-14** — see the Status note below.

## Status — PARKED 2026-06-14 (Needs-sample + Needs-David spike)

Assessed by the Phase-4 loop and **parked, not built** — the entire foundation
(the `/usr/bin/perl` MediaRemote workaround) is unverified on this OS and is a
deliberate spike gate, not a blind build:

- **This machine is macOS 26.5.1** — well past the macOS 15.4 change that
  restricted `MRMediaRemoteGetNowPlayingInfo` to entitled Apple processes. A
  direct call from our binary fails; the only known path is the community
  `mediaremote-adapter` Perl-entitlement workaround, whose survival on macOS 26
  is **unconfirmed** (the brief warns "Apple can move it").
- The brief's build-plan step 1 is **"spike first: run the adapter on current
  macOS, capture real JSON for fixtures."** The now-playing JSON shape is the
  parser's input; building it against an *assumed* (community-crate) shape that
  may be dead/changed on macOS 26 would be parsing blind (cf. the simkl
  fixture-fidelity lesson).
- Running the workaround (download a crate, extract a compiled dylib to a temp
  dir, spawn Perl to load a private Apple framework, circumventing an active
  Apple restriction) is a fragile, security-adjacent operation that warrants a
  deliberate, human-gated spike — not an autonomous loop action.

**To unpark (Needs-David):** run a bounded spike of `mediaremote-adapter` (or
`mediaremote-rs`) on macOS 26 → if it streams now-playing JSON, drop a captured
sample in `~/Trove-samples/` (the Needs-sample) and the loop builds the Live
collector (debounce/session/suppression/mapping + the covered-bundle-ID
double-count guard per the build plan); if the adapter is dead on 26, evaluate
the JXA/osascript fallback, else mark **🚫 unavailable** with the reason.
Time-sensitive: re-verify after each macOS update.

## What it is

Universal now-playing capture for the Mac itself: whatever any app tells
macOS it is playing (Spotify desktop, Music, Safari video, IINA, …)
surfaces through the private MediaRemote framework. This is the Mac-side
sibling of the shipped `nowplaying` def (iPhone Now Playing via Biome) and
the catch-all behind per-service collectors — it sees players Trove has no
integration for.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Now-playing metadata | none (any app that publishes to the system) | title, artist, album, duration, elapsed, playback rate, source app bundle ID | community (crate output) |

All optional in the contract. Coverage depends on apps publishing to the
system now-playing surface — most media apps do; some web players are
inconsistent.

## Access & auth

- Private framework `MediaRemote.framework`. **macOS 15.4+ restricts it to
  entitled Apple processes** — a direct call from our binary fails.
  Workaround (mediaremote-rs): `/usr/bin/perl` carries the
  `com.apple.perl5` entitlement; the crate extracts a compiled dylib to a
  temp dir and spawns Perl to load it, streaming JSON back over stdout.
  Fallback: JXA/osascript (simpler, likely sparser metadata).
- No TCC prompt. Standalone-rule note: the adapter depends on the
  *system* Perl binary's entitlement — not an external app install, so
  standalone-clean, but architecturally fragile (Apple could strip it).
- Lives in `troved` (always-on Live collector); build via
  `scripts/build-troved.sh` to keep TCC grants.

## Vault mapping

- **Raw layer:** none separate — the adapter's JSON is already thin;
  unmapped fields ride in `extra`.
- **Contract layer:** `media/plays/mediaremote/YYYY-MM.jsonl` per the
  ratified media-plays contract — `ts` = play start, `category` inferred
  from source app (`"music"` for music apps, `"video"` for players,
  `"other"` when unknown), `kind:"play"` vs `"partial"` from
  elapsed-at-track-change, `title`/`subtitle` (track/artist), `seconds` =
  observed elapsed, `device` = this Mac, `guid` = hash of (start ts,
  bundle ID, title). Source app bundle ID in `extra`.
- **Overlap guard (the hard part):** Apple Music plays are already
  captured by the shipped `music-scrobbler`; iPhone plays by `nowplaying`.
  Suppress rows whose source bundle ID is covered by a built-in arm
  (e.g. com.apple.Music) so charts don't double-count — the media-plays
  spec explicitly warns against duplicating a built-in arm.

## Build plan

1. **Spike first:** run the mediaremote-adapter Perl path on current
   macOS; capture real JSON for fixtures (this is the Needs-sample). If
   the adapter is dead, evaluate the JXA fallback before abandoning.
2. Module `crates/trove-core/src/mediaremote.rs`: `DEF` (Live), watcher
   that debounces track changes into play rows (start, accumulate elapsed,
   flush on change/stop).
3. Session/dedupe tests with synthetic event streams (skip, pause,
   resume, app-switch); unique temp dirs.
4. Suppression list for covered bundle IDs; verify against a Spotify
   desktop session (the canonical uncovered player).
5. Fragility plan: adapter failure must degrade to a visible "not
   collecting" state on the hub card, never silent rows-stop.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Spotify desktop capture | — | play a track in Spotify on the Mac; confirm a row in `media/plays/mediaremote/` with correct elapsed seconds |
| Double-count guard | — | play in Apple Music; confirm the scrobbler row appears and **no** mediaremote row |
| 15.4+ survival | — | confirm capture on the current macOS version after each OS update (time-sensitive) |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§MediaRemote (Universal Now-Playing) (L3358–L3364). Feasibility 🟡 medium —
the 15.4 entitlement change is real and the Perl workaround, while
functional today (BTT community-confirmed), can break with any macOS
release: **time-sensitive**, re-verify at build and after OS updates. Two
crates implement the workaround (mediaremote-rs, mediaremote-adapter) —
pick at spike time. Complements, never replaces, the shipped `nowplaying`
(iPhone) and `music-scrobbler` (Apple Music) defs.
