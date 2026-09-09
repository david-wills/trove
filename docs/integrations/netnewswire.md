# NetNewsWire

- **id:** `netnewswire`
- **domains:** `reading/` (contract: **Phase 3 pending** — RSS read/starred
  events are reading-shaped; feed-subscription lists stay per-source raw)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (read-only poll of the local SQLite store)
- **connection:** none (local file read; needs Full Disk Access TCC grant)
- **evidence:** official — app is open-source (GitHub:
  Ranchero-Software/NetNewsWire), SQLite schema inspectable in the source;
  container path confirmed in the research doc
- **effort / priority:** M / P2
- **needs:** none (FDA prompt is the existing shared permission flow;
  reading contract not yet ratified — Needs-David at the contract-write step)

## What it is

The flagship free, open-source, macOS-native RSS reader, with a large Mac
user base. Its local SQLite store holds the user's feed subscriptions,
article metadata/content, and read/starred state — a clean record of what
the user follows and actually reads. The only macOS RSS reader with an
open-source, inspectable schema, which is why it's the reference build for
the local-RSS pair (Reeder follows it).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Read/starred article state | free (no tiers) | article URL, title, feed, read/starred flags, timestamps | open-source schema |
| Article metadata + content | free | summary/content, author, published date | open-source schema |
| Feed subscriptions | free | feed URL, title, folder | open-source schema; OPML export fallback |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths.

## Access & auth

- Local SQLite at `~/Library/Containers/com.ranchero.NetNewsWire-Evergreen/
  Data/Library/Application Support/NetNewsWire/Accounts/<account>/` — the
  app is sandboxed, so reading its container requires **Full Disk Access**
  (same TCC grant the existing local collectors use).
- One subfolder per account type (`OnMyMac`, `Feedbin`, `Feedly`, …) — the
  collector must enumerate account subfolders, not assume `OnMyMac`.
- Read-only: open the DB with SQLite read-only/immutable flags (the pattern
  the chrome-history/safari-history defs already use) — never write to or
  lock the app's live DB.
- OPML export (File → Export Subscriptions) is the feed-list-only fallback;
  not worth a separate import path given the DB covers it.
- Standalone-clean: no network, no external app dependency at runtime (we
  read files the user's own app produced; absent app = no data, card shows
  no-data state, never an error).

## Vault mapping

- **Raw layer:** `reading/netnewswire/raw/YYYY-MM.jsonl` — article rows
  joined with feed metadata, full fidelity, partitioned by month of the
  article timestamp; feed-subscription snapshots in
  `reading/netnewswire/feeds.jsonl` (per-source raw, list-shaped).
- **Contract layer:** `reading/netnewswire/YYYY-MM.jsonl` per the (pending)
  reading contract — expected shape: one row per read/starred article
  (`ts`, `source`, `guid` = article id (stable per the schema; else
  hash of feed+URL), `url`, `title`, `feed`, `read`/`starred` flags),
  account name and overflow in `extra`.
- **Dedupe:** article unique id as `guid`; high-water mark per account DB in
  `.trove/netnewswire-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/netnewswire.rs`: `DEF` (Periodic; permission
   hook reports FDA state so the hub card shows the grant affordance).
2. Registration line in `INTEGRATIONS`; no connection.
3. Inspect the schema from the NetNewsWire open-source repo (no fresh
   research needed beyond the pinned source tree at build time per the
   Phase 4 loop); build fixture DBs in tests (unique temp dirs) covering
   multiple account subfolders and read/starred permutations.
4. Read-only SQLite open; tolerate the app running (WAL).
5. Vault writes via `store` helpers once the reading contract is ratified;
   raw layer can land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Read/starred state | ✅ unit-tested | with NetNewsWire installed + FDA granted, read/star an article in the app; Sync now; confirm the row in `reading/netnewswire/` + hub last-data |
| Multi-account subfolders | ✅ unit-tested | add a second account (e.g. Feedbin) in the app; confirm both accounts' articles arrive with the account in `extra` |
| Feed list | ✅ unit-tested | confirm `feeds.jsonl` matches the app's subscription list |

## Build notes (Phase 4)

- Schema confirmed from open-source: Ranchero-Software/NetNewsWire (Articles.swift, StatusesTable.swift, ArticlesDatabase.swift).
- DB file: `DB.sqlite3` per account subfolder under the NNW sandbox container.
- Account subfolders named `{typeRawValue}_{accountID}` (e.g. `OnMyMac`, `2_<uuid>`); collector enumerates all.
- `dateArrived` is the watermark (Unix epoch seconds, integer); `datePublished` is REAL (may be NULL).
- Feed names/URLs are not stored in DB.sqlite3 (they live in memory from Subscriptions.opml); only feedID is available from the DB — carried as `Item.feed` and `extra.feed_id`.
- contract_mode: reuse-bound (reading.Item); raw layer unconditional.
- No new deps needed; rusqlite already in Cargo.toml.

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption"
§NetNewsWire (Local RSS Reader) (L1584–L1591). Feasibility 🟢 high. Build
this one first of the local RSS readers — open schema, SQLite, free app —
then Reeder reuses the reading-contract mapping. If the user syncs
NetNewsWire with Feedly/Feedbin, the local DB already reflects that history,
which partly de-duplicates the cloud-reader briefs (note overlap in the
reading-domain dedupe pass).
