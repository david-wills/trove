# Bitcoin Wallet (Blockstream)

- **id:** `bitcoin`
- **domains:** `finance/purchases/` — **first collector of the
  `finance-purchases` domain** (`finance-purchases.line-item`, **RATIFIED**
  this build: Rust type `LineItem`, registered in `DOMAINS`, fixture promoted
  draft → ratified). On-chain value transfers are recorded as dated purchase
  **line items** (the enrichment layer beside the canonical `finance/` ledger),
  *not* canonical ledger rows — the Phase-2 brief assumed `finance/`; the build
  bound `finance-purchases/` instead (see Vault mapping). The sibling
  `finance-holdings` contract stays a draft until a holdings source binds it.
- **status:** 🧪 built (fixture-tested; live-validation steps below)
- **unavailable_reason:** none
- **behavior:** Periodic — **hourly** (`BITCOIN_SYNC_SECS = 3600`,
  `every_on_run`: the timer only advances when it actually runs, so
  re-enabling fires immediately); poll per-address transaction lists, watermark
  cursor on the last seen txid/block in `.trove/bitcoin-sync.json`.
- **connection:** `bitcoin` — a **keyless `TokenPaste`** connection (the
  "token" is the user's PUBLIC Bitcoin address(es), comma/space-separated,
  never a private key). There is no account/API key; the paste field exists
  only to capture the addresses (and an optional `|https://your-esplora/api`
  host override). Verified with a cheap first-page probe on connect; stored
  0600 at `.trove/sync/bitcoin.json`. (The listenbrainz/boardgamegeek
  precedent: keyless sources still use a TokenPaste `ConnectionDef`.)
- **evidence:** official-docs — Blockstream Esplora REST API
  (blockstream.info/api), open-source and self-hostable; mempool.space
  offers the same REST format as an alternative
- **effort / priority:** S / P1
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement; querying discloses wallet addresses to Blockstream,
  though Blockstream states no persistent logging — say so on enable)

## What it is

Full transaction history for any Bitcoin address via Blockstream's free,
keyless Esplora API. The simplest crypto integration in the catalog: no
credentials at all — the user pastes wallet **addresses** (never private
keys) and Trove reads the public chain. Pairs with the Ethereum/Etherscan
integration to cover ~95% of crypto users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Address transactions | none (no account) | txid, inputs/outputs, value, fee, block time | official docs (`/address/{addr}/txs`) |
| UTXO detail | none | full input/output model per tx | official docs |
| Address stats/balance | none | funded/spent totals | official docs |

No tiers exist — the API is public. All fields land in the ledger row or
its `extra`.

## Access & auth

- REST: `https://blockstream.info/api/address/{addr}/txs` (paginated).
  Alternative host with the same shape: `https://mempool.space/api/…`.
- No API key, no rate-limit contract documented for the public instances —
  poll politely (periodic personal pull is trivial load).
- No TCC, no local files. Standalone-clean (plain HTTPS, zero credentials).
  Privacy: queries disclose addresses to the API host; Esplora is
  open-source and self-hostable for users who care — a custom-host field is
  a cheap escape hatch worth offering.

## Vault mapping

**As built, this binds `finance-purchases`, not the canonical `finance/`
ledger** (the brief above assumed `finance/`; the build chose the purchases
enrichment layer so a wallet's value transfers read as itemized spending and
never collide with bank/card ledger rows). Two layers, month-partitioned by
the local month of `ts`:

- **Raw layer:** verbatim Esplora responses under
  `finance/purchases/bitcoin/raw/` — full source fidelity (the per-source raw
  convention of the `finance-purchases` contract), including the exact satoshi
  values and counterparty addresses.
- **Contract layer:** `LineItem` rows in
  `finance/purchases/bitcoin/YYYY-MM.jsonl` — `guid` = txid, `ts` from block
  time (UTC → local), `merchant` set, amount = the **net value change** to the
  user's wallet across the supplied addresses, computed from the UTXO
  inputs/outputs (signed; satoshis handled without floats). The exact satoshi
  amount, fee, and counterparty addresses ride in each row's `extra` (and the
  raw layer). Required fields: `ts`, `source`, `guid`, `merchant`.
- **Dedupe:** txid as `guid`; the incremental cursor lives in the non-secret
  `.trove/bitcoin-sync.json` (rebuildable by scanning the output files) and
  advances only after a full drain. Cash App's exported Bitcoin rows overlap
  this source — reconciliation on txid happens at **read** time, never merged
  at write time.

## Build plan

> **Built 2026-06-15 (as-shipped deltas from this plan):** the addresses are
> captured via a real keyless **`TokenPaste` `CONNECTION`** (`bitcoin`) — not
> a bespoke config — so the connect/disconnect/status/Sync-now machinery is
> registry-driven (the listenbrainz/boardgamegeek keyless-TokenPaste pattern);
> and the data binds **`finance-purchases`** (`LineItem`, ratified) rather than
> the canonical `finance/` ledger (see Vault mapping). Cadence is hourly. The
> rest of the plan shipped as written (xpub out of scope v1; opt-in privacy
> gate; pagination; fixtures from documented Esplora shapes).

1. Module `crates/trove-core/src/bitcoin.rs`: `DEF` (Periodic), `pull` hook
   for Sync-now; address list (and optional custom Esplora base URL) as
   per-user config, captured via the keyless `TokenPaste` `CONNECTION`.
2. Registration line in `INTEGRATIONS`.
3. Fixtures from documented Esplora response shapes (simple receive, send
   with change output, multi-input tx); net-value computation + ledger +
   cursor tests, unique temp dirs.
4. Pagination for busy addresses; xpub-level scanning is out of scope for
   v1 (plain addresses only — document this in the setup copy).
5. Privacy gate: opt-in with explicit acknowledgement (financial detail +
   address disclosure to the API host).

## Validation matrix

Status legend: 🧪 = fixture-tested (built, green in `cargo test -p trove-core`);
✅ = live-validated by David. **All three rows below are 🧪 — they flip to ✅
once David runs the live steps with a real public address** (the gate validated
the mapping/persist/dedup logic against stubbed Esplora fixtures, never the
network).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Address history → `finance-purchases` | 🧪 | Open the **Bitcoin Wallet (Blockstream)** connect card → paste one or more of your **public** Bitcoin addresses (comma- or space-separated; placeholder `bc1q… , 3J98t… , 1A1zP…`) → **never** a private key/seed. Enable the (default-off) toggle, **Sync now** → confirm `LineItem` rows in `finance/purchases/bitcoin/YYYY-MM.jsonl` (each with `ts`, `source` = `bitcoin`, `guid` = txid, `merchant`, a **signed** net amount; exact satoshis + fee + counterparty addresses in `extra`) + the lossless `finance/purchases/bitcoin/raw/` + the hub "last data" date. **Sync now a second time** → row count unchanged (txid `guid` dedup; the `.trove/bitcoin-sync.json` watermark held). |
| Self-hosted / alternative host | 🧪 | Re-connect appending `\|https://mempool.space/api` (or your own Esplora) after the addresses → Sync now → confirm identical rows (same txids), proving the host override parses and the privacy escape hatch works. |
| Cash App overlap (read-time) | 🧪 | A user with Cash App BTC activity has both sources connected → confirm the read-time view reconciles on txid and does **not** double-count (the chain row in `finance/purchases/bitcoin/` and any Cash App row stay distinct records joined at read time, never merged at write time). |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Bitcoin
Blockchain (Blockstream Esplora API) (L3795–L3801). Feasibility 🟢 high —
"completely keyless public API… no account needed." Blockstream states no
persistent logging/tracking; self-hosted Esplora is the paranoid path.
Solana deferred: Helius requires a paid plan for address-history queries
(launched Oct 2025) — public RPC fallback is a possible M6 later.
