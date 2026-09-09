# Plan: Financial Integrations

*2026-06-11 — drafted in the `financial-integrations` worktree after a decision discussion. Companion to `docs/integrations-hub-report.md` (registry doctrine) and `docs/data-sources.md`.*

## Goal

Bring bank, credit-card, and (eventually) merchant-level purchase data into the vault — the Copilot/Mint/Monarch capability, done Trove-style: files as source of truth, local-first, distributable to anyone, no Trove-owned server.

Decisions made with David (first user, not design target):

- **Aggregator is acceptable as an explicit opt-in integration.** The "no cloud calls" rule bends only for user-enabled connectors, consistent with the existing TickTick precedent.
- **v1 backends: SimpleFIN + file import.** Aggregator-agnostic schema; other backends (Amazon, Plaid/Teller, FinanceKit companion) come later.
- **Fully automatic sync** via troved once connected.
- **v1 data: transactions + balances.** Categorization/analytics, Amazon order enrichment, and investments/holdings are explicit follow-ups (in roughly that order; investments deferred furthest).
- **Maximize history**: Copilot Money CSV import seeds years of backfill on day one; bank-statement CSV importers extend it.

## Why SimpleFIN as the first aggregator

Bank aggregation structurally requires a third party (banks don't offer consumer APIs). The realistic candidates:

| | SimpleFIN Bridge | Teller | Plaid |
|---|---|---|---|
| Cost | $1.50/mo per user | free (100-connection dev tier) | free trial (10 Items) |
| Relationship | **user ↔ SimpleFIN** | developer ↔ Teller | developer ↔ Plaid |
| Trove code | tiny (poll one endpoint) | medium (Connect UI + mTLS) | medium-large (Link + approval) |
| Freshness | ~daily | near-real-time | near-real-time |
| Coverage | broad (MX network, ~16k institutions) | good, gaps in small CUs/fintech | broadest |

The decisive row is **relationship**. Teller/Plaid issue *developer* keys: every user of a distributed Trove would ride on our quota and ToS, and the keys would have to ship in the binary (extractable) or live on a Trove relay server (violates local-first). SimpleFIN is the only one where each user brings their own credential: they sign up at `bridge.simplefin.org`, pay their own $1.50/mo, connect banks through MX's hosted widget, and paste a one-time **setup token** into Trove. Trove claims it once (POST → long-lived access URL), then polls `GET {access_url}/accounts` — the entire protocol. This is why the local-first ecosystem (Actual Budget et al.) standardized on it. Honest framing for the card copy: *SimpleFIN's fee is the price of Trove not having a cloud.*

Designing the schema and sync engine aggregator-agnostically keeps Plaid/Teller available as alternate backends later without schema churn.

## Coverage reality (what connects, what doesn't)

- **Banks & credit cards** (Chase, BofA, Amex, Capital One, …): solid via MX. This is the core network.
- **Brokerage/retirement**: connects (mostly official OAuth, e.g. Fidelity Access); balances + transactions reliable; holdings come through the Bridge but are semi-documented protocol extensions. Employer 401(k)s are hit-or-miss on *every* aggregator. Deferred anyway.
- **Apple Card / Cash / Savings**: **no aggregator can reach these — anywhere.** Apple only exposes them via FinanceKit, an on-device iOS API with Apple-granted entitlement (how Copilot/Monarch/YNAB do it, via their iPhone apps). Desktop routes: Wallet's monthly CSV export (→ file import), or an iOS companion app someday.
- **Venmo / Cash App**: walled gardens for everyone; they consume Plaid, they don't provide. Activity surfaces indirectly as transfers on linked bank/card accounts; CSV export for detail.
- **Amazon**: no API. Copilot's integration = user logs into Amazon *inside the app*, it scrapes their own order history and matches orders to card transactions (amount equal, dates within ~2 days). Trove can do the same **more privately** — our logged-in-session snapshot machinery (`browser.rs`, used for Audible/podcasts) runs on the user's machine, not our cloud. Fast-follow, not v1.

Conclusion baked into the design: **file import is not a fallback, it's a peer backend** — it's the *only* route to Apple Card/Venmo/Cash App for any desktop app, and serves users who refuse aggregators on principle.

## Vault format

Files are the source of truth; everything below is plain CSV/JSONL, rebuildable-index rules apply.

```
~/Documents/Trove/finance/
  accounts.jsonl                    # registry: one line per known account
  transactions/<account-id>/<year>.jsonl
  balances/<account-id>.jsonl       # daily balance snapshots (net-worth-over-time)
  imports/                          # drop folder; troved/app watches it
  imports/archive/                  # processed files moved here, never deleted
```

**Canonical transaction record** (every backend normalizes to this):

```json
{
  "id": "…",              // stable per source; see identity below
  "account": "…",          // vault account id
  "posted": "2026-06-10",
  "transacted": "2026-06-09",   // optional
  "amount": "-42.17",      // signed decimal string, never floats
  "currency": "USD",
  "description": "AMZN Mktp US*…",
  "payee": null,           // normalized later by categorization pass
  "category": null,        // null until categorization phase
  "pending": false,
  "source": "simplefin",   // simplefin | csv-import | copilot | amazon | …
  "extra": { }             // source-specific passthrough
}
```

**Account identity:** vault account ids are Trove-assigned slugs; each account record carries per-source aliases (SimpleFIN account id, Copilot account name, statement-CSV fingerprint) so multiple backends land in the same account. First sight of an unknown source account → auto-create + surface in UI for optional rename/merge.

**Transaction identity & dedup** — the hardest real problem, three tiers:

1. **Within SimpleFIN**: stable per-account transaction ids; idempotent upsert. Pending transactions are upserted with `pending: true`; each sync replaces the pending set for the synced window (pending→posted often changes id — treat pendings as ephemeral until posted).
2. **Within file imports**: no ids; synthesize `hash(account, posted, amount, normalized-description, occurrence-counter)` — counter handles same-day identical transactions deterministically.
3. **Across sources** (Copilot backfill overlapping SimpleFIN's first-connect window; statement CSV overlapping both): fuzzy match — same account, equal amount, posted within ±3 days, similar normalized description. On match, the aggregator record wins as canonical and absorbs the import's extras (notably Copilot's category). Unmatched import rows insert normally. Matching runs at import time, not sync time, so the daily path stays trivial.

**Credentials:** the SimpleFIN access URL is a bearer credential — macOS Keychain, never the vault. The one-time setup token is burned at claim and never stored.

## Engine

New `crates/trove-core/src/finance/` module:

- `simplefin.rs` — token claim, accounts fetch, normalize, upsert. The protocol is small enough that a hand-rolled client over the existing HTTP stack is right; no SDK.
- `import.rs` — drop-folder watcher + format detection + parsers: Copilot export, generic CSV (header-mapping), OFX/QFX (banks still offer these and they parse unambiguously), then per-bank presets (Chase/Amex/BofA/Apple Card Wallet export) added as encountered.
- `model.rs` — canonical types, identity, dedup/merge.

**Scheduling:** troved polls SimpleFIN once daily (the Bridge updates ~daily; more is wasted) plus on-demand "Sync now". Import watcher runs in both app and daemon like other watchers.

**Registry entries** (one catalog entry each, per hub doctrine):

- `bank-sync` — `CloudSync`. Setup steps walk through the SimpleFIN signup + token paste; caveats state plainly: third-party service, ~$1.50/mo, read-only, once-daily data, credentials never touch Trove.
- `finance-import` — `Import`. Drop-folder path, supported formats, per-source caveats (Apple Card export lives in Wallet on iPhone; Venmo/Cash App CSV locations).

**UI:** hub cards via the registry; a Finance tab (accounts list, balances, transaction browser) can start minimal — the vault files are the product in v1.

## Phases

1. **Core + SimpleFIN** — schema, `finance/` module, token flow + Keychain, troved daily sync, registry entries + hub cards, minimal Finance tab. *This alone matches the baseline of Mint/Copilot for banks & cards.*
2. **File import + backfill** — drop folder, Copilot importer (seeds years of history + categories), generic CSV mapper, OFX/QFX, cross-source dedup.
3. **Categorization & analytics** — payee normalization; rules engine seeded from Copilot's imported categories; recurring/subscription detection; monthly reports. (Local rules first; any model-assisted categorization stays on-device per standalone rule.)
4. **Amazon enrichment** — order-history scraping via the browser snapshot machinery; order↔transaction matching (amount + date window); line items attach to transactions.
5. **Later** — investments/holdings (validate Bridge holdings data), alternate aggregator backends (Plaid/Teller for users who prefer them), iOS companion for FinanceKit (Apple Card done properly).

## Risks

- **SimpleFIN is a small company.** Mitigated by the agnostic schema (swap backends without data migration) and by file import as a permanent floor.
- **Connection breakage** (bank MFA resets, OAuth expiry) is endemic to all aggregators. Trove's job: surface staleness honestly ("Chase last updated 6 days ago") and deep-link to the Bridge dashboard — repairs happen there, not in Trove.
- **Cross-source dedup** errors either duplicate or silently merge distinct transactions. Bias the matcher conservative (duplicates are visible and fixable; silent merges aren't), and log every cross-source merge into the record's `extra`.
- **Amazon scraping fragility** — page-structure drift; standard for the snapshot integrations, change-gated like podcasts.
- **Float poisoning** — amounts are decimal strings end-to-end; never `f64`.

## Plausibility verdict

High. Phase 1 is a small, well-bounded lift (the SimpleFIN protocol is deliberately tiny, and the hub/troved/keychain scaffolding all exists). Phases 2–3 reach Copilot-grade tracking. Amazon enrichment is proven feasible (Copilot does it with worse privacy properties than we'd have). The only things genuinely out of desktop reach are FinanceKit (needs iOS) and Venmo/Cash App live sync (out of reach for everyone).
