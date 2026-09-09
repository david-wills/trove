# Google Integration Plan

*Created 2026-06-11, decided with David. Google is **one integration entity** (flagged in the calendar worktree; doctrine here). This doc is the design + the API landscape research; per-source rows live in `docs/data-sources.md`.*

---

## Decisions (locked with David, 2026-06-11)

| Decision | Choice |
|---|---|
| Auth model | **David's own Google Cloud OAuth client now, Google verification later.** Client ID read from config (the `docs/oauth-distribution.md` model: compiled-in for official builds, BYO always available) — a verified shared client swaps in later with zero code changes. |
| Scopes | **Minimal, read-only — but the full v1 bundle requested up front, in one consent** (revised 2026-06-11: supersedes the earlier "incremental, per-service" intent). Connect asks for `openid email profile` + the six read-only service scopes together, so the user consents once instead of re-consenting per service. Still no preemptive Drive/Docs scopes — those stay out until there's a feature for them (incremental auth remains the path for *new* scopes added in future versions). The set of *registered* restricted scopes drives the verification audit regardless of when they're requested, so bundling doesn't enlarge it. |
| v1 services | **Gmail, Calendar, Contacts (incl. "Other contacts"), Tasks, YouTube (subs/playlists/likes), Books My Library.** |
| Multi-account | First-class: any number of Google accounts connect independently, each with its own token set; account address keys the data (the mbox importer's `account`/`service` pattern). |
| Email depth | **Lean default + per-account opt-in toggles.** Default = the existing `correspondence/email/` stream (full text bodies, attachment *metadata*). Toggles: archive raw RFC-822 (`.eml`) · download attachments. Toggling on later can re-fetch history. |
| Mail scope | **Everything except Spam/Trash**, Gmail category labels preserved (human-correspondence is a read-time filter) — plus per-account category exclusions (Promotions, Social, any label) for users who want a smaller vault. |
| Calendar overlap | **Same store as EventKit, dedupe by Google event ID** (it survives the EventKit round-trip). One unified calendar however an event arrives; reschedule/cancel diffs keep flowing to `calendar/changes/`. |
| Docs/Sheets/Drive | **Deferred, user-initiated someday.** Trove is a trove of *complete* data, not in-progress drafts. |
| Data Portability API | **On the roadmap, region-gated** (see below). Not v1. |
| Google Health API | **Follow-on integration** (own card; Oura-scale value). Not part of the Google services entity's v1, but shares the OAuth client. |

---

## Auth detail

- **Flow:** Google "Desktop app" OAuth client → PKCE + loopback redirect, exactly what `sync/oauth.rs` already does (Google accepts any loopback port for desktop clients — no console-side redirect registration needed, unlike TickTick/Oura). Refresh tokens are standard (the foundation's refresh path finally gets exercised).
- **The restricted-scope reality:** `gmail.readonly` is in Google's *restricted* tier. A shared client serving arbitrary users requires Google verification **plus an annual CASA security assessment** (weeks–months, recurring cost). Until then: David's client runs the consent screen in **Testing** mode with his accounts as test users; other users paste their own client ID (BYO form, same as TickTick).
- **Testing-mode caveat (UI must handle):** while the consent screen is in Testing, **refresh tokens expire after 7 days** → weekly reconnect. Surface as a "reconnect" state on the card (disabled-controls-need-affordance), never a silent failure.
- **Setup (David / any BYO user):** Google Cloud Console → new project → enable Gmail API, Calendar API, People API, Tasks API, YouTube Data API v3, Books API → OAuth consent screen (External, Testing, add own addresses as test users) → Create credentials → OAuth client ID, type "Desktop app". (~10 min; becomes the card's `setup` steps.)

## v1 services → vault mapping

| Service | API | Read scope(s) | Vault target |
|---|---|---|---|
| Gmail | Gmail API | `gmail.readonly` | `correspondence/email/YYYY-MM.jsonl` (existing stream, existing Message-ID dedupe — coexists with mbox imports). Full-history backfill on connect; incremental via the **`historyId` cursor**. Opt-in raw `.eml` / attachments land beside the stream (layout decided at build time). |
| Calendar | Calendar API | `calendar.readonly` | `calendar/events/` + `calendar/changes/` (existing stores), deduped against EventKit by Google event ID. Incremental via **sync tokens** — a moved meeting updates promptly even when Apple Calendar isn't configured. |
| Contacts | People API | `contacts.readonly`, `contacts.other.readonly` | `contacts/` (new; the entity backbone from catalog §3). **"Other contacts"** — the auto-collected everyone-you've-emailed list — is arguably the bigger prize: a complete interaction graph that lets correspondence senders resolve to people. |
| Tasks | Tasks API | `tasks.readonly` | `tasks/google-tasks/` via the existing normalized contract (snapshot + events), like TickTick/Reminders. Completed + hidden tasks included. |
| YouTube | YouTube Data API v3 | `youtube.readonly` | New store (media-adjacent): subscriptions, playlists (incl. liked videos), own uploads. **Watch history is NOT in the API** (dead since 2016) — Takeout or Data Portability only. |
| Books | Books API | `books` | Bookshelves (Have Read / Reading Now / To Read / Favorites) + **Play Books annotations** (highlights/notes) — the cloud sibling of `books.rs` (Apple Books). Reading history is complete data, squarely in scope. |

## Follow-on: Google Health API

The **rebuilt Fitbit Web API on Google OAuth** (the legacy Fitbit API + Fitbit auth sunset **September 2026** — new work targets this directly). Steps, distance, active minutes, heart rate, HRV, sleep, SpO₂, respiratory rate, temperature from **all Fitbit devices and Pixel Watches**; blood pressure/women's-health/mindfulness due Q3 2026. Slots into `health/` beside Oura — same category, second device ecosystem, and `oura.rs` is the template (keyed upserts, resumable backfill). Caveat: every scope requires Google's privacy/security review approval, even for personal use. Own integration card; shares the Google OAuth client.

## Later: Data Portability API (region-gated)

An **automated Takeout**: OAuth-consented exports with **recurring access grants (30 or 180 days, refreshable every 24h)** — built for exactly a Trove-shaped app. Covers what no per-service API reaches:

- **`myactivity.*`** — Search history, **YouTube watch/search activity**, Maps activity, Play activity, Shopping activity, My Ad Center
- **Chrome** — history, bookmarks, reading list, autofill, extensions, settings, dictionary
- **Maps** — reviews, starred/labeled places, contributed photos/Q&A, commute settings; **MyMaps**
- **Play** — installs, purchases, subscriptions, library, devices, Play Points
- **Search UGC** — movies/TV marked watched, thumbs ratings, media reviews
- Plus: Discover follows/likes, Google Alerts, Saved collections, Street View uploads, Order & Reserve (food orders/reservations), Fitbit device events, YouTube comments/posts/live-chat/music-library

**Why not v1:** available **only in the EU and UK** (DMA compliance; Google "exploring" expansion, no timeline) and not for under-18 accounts. David's US account can't use it. Build the Google integration so this lights up as a region-gated source later — the OAuth plumbing is identical; the time filter + recurring-grant flow is the only new machinery. Re-verify availability before building.

## Dead ends (manual Takeout remains the only route)

Photos library (Library API stopped allowing full-library reads March 2025; the Picker API is user-selected-items only), Maps Timeline / Location History (fully on-device since Dec 2024 — **no API at all**), Keep (API is Workspace-enterprise-only), Google Pay/Wallet transactions, Assistant/Gemini activity, Chrome sync data and watch history outside the EU/UK portability route, YouTube Music listening history.

## Build order within the entity

1. **OAuth plumbing** — Google `Provider` in `sync/`, multi-account token store, reconnect-state surfacing.
2. **Gmail** (the behemoth): backfill + `historyId` incremental into `correspondence/email/`, category exclusions, depth toggles.
3. **Calendar** (sync tokens + EventKit dedupe), **Contacts**, **Tasks** — each small once auth exists.
4. **YouTube**, **Books** — new stores, small APIs.
5. Google Health API as its own follow-on integration; Data Portability when region-feasible.

Every piece follows the integrations contract: `INTEGRATIONS` entry with setup steps + caveats (7-day testing-mode reconnect, watch-history/Photos/Timeline gaps), toggles gated via `integration_enabled`, and a verification-grade tab/section per source.
