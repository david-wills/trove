# Reeder

- **id:** `reeder`
- **domains:** `reading/` (contract: **Phase 3 pending** — same reading
  shape as NetNewsWire; build after it and reuse the mapping)
- **status:** 🧪 Reeder 5 parser implemented (Classic deferred)
- **unavailable_reason:** none
- **behavior:** Periodic (read-only poll of the local database)
- **connection:** none (local file read; needs Full Disk Access TCC grant)
- **evidence:** live sample confirmed — opened 13 MB Reeder 5 default.realm via
  realm-db-reader v0.2.1 (pure Rust); 3801 articles, 45 feeds successfully parsed.
- **effort / priority:** M / P2
- **needs:** Reeder Classic install for the Classic SQLite reader (deferred)
- **schema confirmed:** extId, deleted, unread, starred, readLater, publishedDate,
  starredDate, readLaterDate, link, title, author, summary, thumbnail, content,
  htmlContent, fullContent, fullTitle, fullHtmlContent, user, feed

## What it is

Reeder is a popular paid (~$9.99) macOS/iOS RSS reader. It syncs with
Feedbin, Feedly, iCloud and others, so its local database reflects the
user's *full* reading history across services — read state, starred items,
feed subscriptions — making it a one-stop local read for users whose cloud
reader is otherwise paywalled (see the Feedly/Inoreader briefs).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Read/starred article state | one-time paid app, no tiers | article URL, title, feed, starred (`starred == 1`), read state, timestamps | community-inspected (medium) |
| Feed subscriptions | — | feed URL, title | community-inspected (medium) |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths.

## Access & auth

- **Reeder 5:** Realm DB at `~/Library/Containers/com.reederapp.5.macOS/
  Data/Library/Application Support/default.realm`. Realm, not SQLite —
  needs a Rust Realm reader (`realm-rs` exists in the ecosystem) or a
  custom parser. Standalone rule: whatever reader is used must compile
  into the binary as a library — no helper apps, no Realm tooling
  dependency at runtime.
- **Reeder Classic:** SQLite at `~/Library/Containers/com.reederapp.macOS/
  Data/Library/Application Support/`. Easier path; detect by bundle
  container presence and support both.
- Both containers are sandboxed → **Full Disk Access** (the existing shared
  TCC flow). Read-only open; tolerate the app running.
- Schema is undocumented for both versions — fields above are
  community-inspected, so the parser is built last, against real sample
  DBs (Needs-sample).

## Vault mapping

- **Raw layer:** `reading/reeder/raw/YYYY-MM.jsonl` — extracted article
  records, full fidelity, partitioned by article timestamp month; feed
  list snapshot in `reading/reeder/feeds.jsonl`.
- **Contract layer:** `reading/reeder/YYYY-MM.jsonl` per the (pending)
  reading contract — same row shape as NetNewsWire (`ts`, `source`,
  `guid`, `url`, `title`, `feed`, `read`/`starred`), Reeder version +
  sync-backend in `extra`.
- **Dedupe:** stable article id from the DB as `guid` (fallback: hash of
  feed+URL); watermark in `.trove/reeder-sync.json`, rebuildable.

## Build plan

1. Sequence **after NetNewsWire ships** — it proves the local-RSS →
   reading-contract mapping on a documented schema first.
2. Module `crates/trove-core/src/reeder.rs`: `DEF` (Periodic; FDA permission
   hook), version detection (Reeder 5 Realm container vs Classic SQLite
   container).
3. **Parser-last (Needs-sample):** acquire a real `default.realm` sample
   (David has no Reeder install on record — any user/tester sample works),
   inspect the schema, then write the parser. Evaluate `realm-rs`
   compile-in viability early; if Realm reading proves impractical, ship
   Classic-SQLite-only and mark Reeder 5 unavailable-with-reason on the
   capability row rather than blocking the whole def.
4. Registration line in `INTEGRATIONS`; no connection.
5. Fixture DBs (both formats once sampled) + parser/store/cursor tests,
   unique temp dirs. Vault writes via `store` helpers once the reading
   contract is ratified.

## Build status (2026-06-17)

- DEF registered: Periodic, hourly, FDA permission check for both containers.
- Both Reeder 5 (Realm) and Classic (SQLite) container paths detected.
- Schema fully confirmed from live `default.realm` (13 MB, ~3801 articles, 45 feeds) opened
  via `realm-db-reader v0.2.1` (pure Rust, MIT).
- Key schema corrections from live probe: `unread`/`starred`/`readLater` are `Int(0/1)` not
  `Bool`; `publishedDate`/`starredDate` are `Float` (Unix seconds) not Realm Timestamps;
  `class_Feed` has no `title`/`name` column — feed names derived from URL host.
- Realm parser IMPLEMENTED: `read_realm5()` opens the live file, reads 3801 articles, joins
  feed names, maps state (starred→"favorite", read→"read", readLater→"saved").
- State mapping corrected: `readLater` (not starred) maps to `"saved"` (not `"favorite"`).
- Reeder Classic (SQLite) reader deferred — no Classic install available for schema inspection.
- 12 tests pass covering DEF metadata, state mapping, dedup, feeds, raw layer, live Realm file.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Reeder 5 (Realm) read/starred | IMPLEMENTED | On a Mac with Reeder 5 + FDA: Sync now; confirm rows in `reading/reeder/YYYY-MM.jsonl` and hub last-data |
| Reeder 5 read_later (saved) | IMPLEMENTED | Mark an article "Read Later" in Reeder; Sync now; confirm `state: saved` row |
| Reeder Classic (SQLite) | DEFERRED | No Classic install available; stub returns Err gracefully |
| Sync-backend coverage | IMPLEMENTED | Reeder synced to Feedly; remote articles appear (confirmed from live sample) |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption"
§Reeder (Local RSS Reader) (L1592–L1599). Feasibility 🟡 medium — the Realm
dependency is the whole risk; SQLite Classic is easy. Bundle ID varies by
version (`com.reederapp.5.macOS` vs `com.reederapp.macOS`). Because Reeder
mirrors Feedbin/Feedly state locally, it can deliver paywalled cloud-reader
history for free — note the overlap in the reading-domain dedupe pass.
