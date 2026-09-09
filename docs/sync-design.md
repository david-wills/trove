# Trove — Sync Design Notes

*Exploratory design captured 2026-06-10. This is **not committed scope** — sync is a far-future, paid-tier idea (see HANDOFF.md §4 "Explicitly deferred"). This doc records the reasoning so it doesn't have to be regenerated when sync is actually specced. Nothing here is built.*

> **Guiding constraint:** "as secure as humanly possible" has a precise meaning here — the sync server (if one exists at all) must be **mathematically incapable of reading your data, and incapable of tampering with it undetected.** True zero-knowledge / E2EE is the bar. Everything below works backward from that, and from Trove's principles: files are the source of truth, data leaves the machine only by explicit user choice, and capabilities are absorbed as compiled-in libraries (never external running apps).

---

## 1. Threat model (define "secure" before claiming it)

For a *permanent personal-data archive*, the adversaries are:

- **Passive network eavesdropper** — trivial to beat (TLS + payload encryption).
- **Malicious / compromised sync server** — the real threat. Assume the cloud is fully hostile, not merely "honest but curious."
- **Lost or stolen device.**
- **Metadata leakage** — file names, sizes, counts, timing, even when content is encrypted.
- **Harvest-now-decrypt-later** — uniquely relevant because Trove data is meant to last *decades*. Ciphertext recorded today must survive future quantum attacks.

Design target: the server stores only opaque blobs + wrapped keys, and can decrypt none of it.

---

## 2. Crypto core — and the three-way tension

Symmetric encryption is the *easy* part (XChaCha20-Poly1305 or AES-256-GCM — settled science). The hard part is key management, governed by a tension you cannot fully escape:

> **Convenience vs. recoverability vs. zero-knowledge — pick two.**

**Master-key shapes:**

- **Passphrase-derived** — vault key = `Argon2id(passphrase, salt)`. Server sees nothing; any new device just needs the passphrase. Downside: forget it → data is gone; passphrase strength *is* the security.
- **Random vault key + device enrollment** (stronger; this is the Apple Advanced Data Protection / Signal model):
  - Each device generates an **X25519 keypair; the private key never leaves the device.** On Mac/iOS, bind it to the **Secure Enclave / Keychain** → hardware-bound, non-exfiltratable even from a rooted device. Big real-world win, free on Apple platforms.
  - One random 256-bit **vault key**, **wrapped (encrypted) to each enrolled device's public key.** The server only ever holds wrapped blobs.
  - **Add a device:** new device shows a QR / short numeric code; an existing device verifies it out-of-band (a **SAS** — short authentication string, like Signal safety numbers), then wraps the vault key to the new device's public key. Server relays ciphertext it can't open.
  - **Remove a device:** rotate the vault key, re-wrap to survivors.

**Pragmatic best-of-both:** random vault key wrapped by **both** device keys *and* a passphrase-derived recovery key. Use **`age`** (Filippo Valsorda's X25519 format; solid Rust crate `age`/`rage`) as the wrapping primitive. **Never hand-roll crypto** — compose vetted primitives (libsodium/`crypto_box`, `age`, Argon2id, HKDF, `ring`). The novelty belongs in the *vault-aware orchestration* in `trove-core`, not in the cipher.

---

## 3. Topology — and the phasing for Trove

Ship in this order:

1. **P2P LAN sync, no server at all** (Syncthing-style block exchange). Your own devices on the same network sync directly, E2E; **no third party ever involved.** This is the literal embodiment of "data never leaves the machine unless the user chooses." → **Sync v1.**
2. **Untrusted relay / dumb ciphertext blob-store** for offline + remote sync and backup. Server is a content-addressed ciphertext store + queue, holds zero keys. → natural shape of the eventual **paid tier**.
3. **Rent dumb storage** (iCloud Drive / S3) with client-side encryption layered on top (Cryptomator / rclone-crypt model) — same trust properties as #2 without running infrastructure.

In all three, TLS is hygiene only; assume it can be stripped and rely on the payload already being E2E encrypted.

---

## 4. File-sync mechanics — where Trove has a structural advantage

Files are the source of truth, so sync = encrypted file reconciliation: **content-defined chunking** (FastCDC), hash chunks, encrypt each, sync only changed chunks (restic/borg style). Conflict resolution is where Trove's data model pays off:

- **Append-only streams (health CSVs, activity JSONL) — the majority of the vault — merge trivially by union + dedupe of rows.** Two devices appending different days just combine. No conflict. A real gift of the files-as-time-series design.
- **Notes are the only true edit-conflict surface:**
  - **Conflict copies** (Syncthing-style `note.sync-conflict-DATE.md`) — never loses data, user resolves. Simplest; fits "the .md file IS the truth." → preferred first cut.
  - **CRDTs** (Automerge/Yjs) — automatic merge, but CRDT state is a separate binary blob that fights the plain-markdown-is-truth principle (would be used as transport only, still materializing plain `.md`). Powerful but heavy; v-later.

→ **Per-data-type merge strategies**, slotting into the modular collector architecture.

---

## 5. The attacks people forget (confidentiality ≠ integrity ≠ freshness)

A server locked out of *reading* can still:

- **Tamper / roll back** — serve stale or forged state. Beat with AEAD on every chunk **plus a signed Merkle root / version vector**: a signed "head" pointer each device verifies. Devices should **gossip the latest signed root peer-to-peer** so the server can't **fork** two devices into divergent views (the classic untrusted-storage attack).
- **Leak metadata** — sizes, counts, timing. Mitigate: **encrypt file paths/names** (server sees opaque IDs, not `health/heart-rate/2026-06.csv`), **pad to size buckets**, fixed-size chunks to hide the file tree. Be honest: perfect metadata privacy is expensive and never quite complete.
- **Future quantum** — wrap keys with a **hybrid KEM (X25519 + ML-KEM/Kyber)** so today's ciphertext stays safe for decades. Symmetric layer (AES-256) is already quantum-fine. Matters more for Trove than for ephemeral chat precisely because the archive is permanent.

---

## 6. Recovery — the part with no clean answer

True zero-knowledge means: **lose every device and forget the passphrase → the data is gone, by design.** Any server-side "reset password" is a backdoor that voids the guarantee. Principled options:

- **Printable recovery key** (BIP39 word list of the raw vault key) stored offline — exactly what Apple ADP does.
- **Shamir secret sharing / social recovery** — split into N shares, need K to reconstruct, distribute to trusted people/locations. Strong, real UX cost.

Stance: no backdoor; recovery is the user's responsibility; make that **loud** in onboarding rather than papering over it.

---

## 7. P2P LAN auto-sync across iPhone / iPad / Macs — platform reality

**Can all my Apple devices auto-sync over local P2P LAN, in the background?** Yes — with one sharp asymmetry: **Macs can do it fully and continuously; iPhone/iPad do it perfectly in the foreground but only opportunistically in the background.** iOS forbids persistent background daemons and no cleverness fully escapes that.

### Discovery + transport (the easy part — native)

- **Bonjour / mDNS** for discovery — advertise/browse a `_trove._tcp` service via `NWListener`/`NWBrowser` (**Network framework**), which provides encrypted peer-to-peer connections.
- **Multipeer Connectivity** as an alternative — P2P over Wi-Fi *and* peer-to-peer Wi-Fi (AWDL, the AirDrop radio tech), so devices can sync even when not on the same access point.
- iOS 14+ shows a one-time **Local Network permission** prompt.

### Background reality (this is the crux)

- **macOS — no restrictions.** `troved` under launchd runs 24/7; a Mac advertises, listens, and syncs continuously, forever, in the background.
- **iOS / iPadOS — restricted, not fully beatable:**
  - **Foreground = real-time.** App open → full P2P sync, instant.
  - **`BGTaskScheduler`** (app-refresh / processing tasks) — OS grants *short, opportunistic* windows on *its* schedule, throttled by usage; processing tasks mostly fire when charging + idle. "Catch up in a burst," not continuous.
  - **`HKObserverQuery` + `enableBackgroundDelivery`** — special for Trove: **iOS wakes the app when new HealthKit samples arrive, even in background** → a wake-up exactly when there's new health data to grab and attempt to push.
  - **Local Push Connectivity (`NEAppPushProvider`)** — the *one* sanctioned way to hold a **persistent background network connection without a remote push server.** Built for on-prem messaging/VoIP. Would let an iPhone keep a live LAN sync connection in the background — but **entitlement-gated**, only on **configured Wi-Fi SSIDs**, and genuinely advanced. The "if you really want it" door.
  - **Not possible:** two iPhones, both asleep, silently syncing to each other in the background.

### The architecture this pushes toward — always-on Mac hub

Exactly one device class can be always-on, so make it the spine:

- **Designate a Mac as the always-on LAN hub** — runs `troved`, advertises over Bonjour 24/7, acts as the durable sync node.
- **Mac ↔ Mac:** continuous, automatic, background — effortless.
- **iPhone/iPad ↔ Mac hub:** syncs *every time the app opens* (instant catch-up) + background-refresh bursts + health-data wake-ups. The Mac bridges gaps so phone and iPad never need to be awake simultaneously — phone pushes when it gets a window, iPad pulls when it gets one.

Lived experience: the Mac is always current; opening Trove on the phone shows it instantly up to date, with background top-ups in between. You just don't get *continuous real-time background sync on the iOS devices themselves* without the Local Push Connectivity route.

### Bottom line

- Auto P2P LAN sync **between Macs, fully background:** yes, no caveats.
- iPhone/iPad **foreground:** yes, real-time.
- iPhone/iPad **background:** best-effort bursts + health-triggered wake-ups, *not* continuous — unless you adopt the entitlement-gated Local Push Connectivity extension.
- Clean design: an **always-on Mac hub** absorbs the iOS background limits.

---

## 8. One-line summary

**Start with hardware-key-bound P2P LAN sync that no server ever touches (Macs as the always-on hub), and only later add an untrusted ciphertext relay for the remote/paid tier — with signed Merkle roots for anti-rollback and hybrid post-quantum key wrapping, because this archive is meant to outlive its own threat model.**

---

## 9. Build posture (when this is eventually picked up)

- **Never invent crypto.** Compose vetted primitives; novelty lives in `trove-core` orchestration.
- **Absorb sync engines as libraries, not running apps** (principle #3): Syncthing block-exchange protocol, Cryptomator vault format, `age` — lift as compiled-in code, not an external daemon.
- **All sync I/O still goes through `trove-core`**, reused by both the GUI app and `troved`.
- Sync depends on solving secure key management first; iOS as a full Trove node depends on sync existing (see HANDOFF.md §1 iOS analysis).
