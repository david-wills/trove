# Trove — Integrations Research

*Generated 2026-06-11 by a 17-domain parallel research sweep (18 agents, ~1.1M tokens). An exhaustive feasibility map of every data source Trove could collect, judged against the hard constraints: local-first, files-as-truth, **standalone** (no runtime dependency on an external app/service), built-for-anyone, macOS. Companion to `docs/data-sources.md` (the build checklist) — this is the wide net; that is the working order.*

**396 sources** researched across **17 domains**. Feasibility: 🟢 High · 🟡 Medium · 🟠 Low · 🔴 Blocked. Status: ✅ built · 📋 planned · 🆕 new.

**Mechanisms:** M1 one-shot file import · M2 watch folder · M3 local DB copy-then-read (often needs Full Disk Access) · M4 OS API watcher (TCC) · M5 cloud API pull (OAuth/token) · M6 agent collector.

> **⚠️ Vault paths in this doc are not authoritative.** Per-source entries
> below suggest vault paths (`geo/…`, `~/Trove/social/dating/`, …) that
> predate the Phase 1 domain taxonomy. **The taxonomy table in
> `docs/integrations/README.md` wins wherever they disagree.** Phase 2
> briefs take their paths from the taxonomy, never from this doc.

---

## Contents

1. [Executive Summary](#executive-summary)
2. [Top Recommendations (prioritized)](#top-recommendations)
3. [Reusable Mechanism Investments](#reusable-mechanism-investments)
4. [Aggregator Hubs](#aggregator-hubs)
5. [Hard Blocks](#hard-blocks)
6. [Cross-Cutting Themes](#cross-cutting-themes)
7. Domain catalogs:
   1. [Email & Messaging Apps](#email-messaging-apps) — 29 sources
   2. [Calls, Voice & Meeting Transcripts](#calls-voice-meeting-transcripts) — 18 sources
   3. [People, Contacts & Relationship Graph](#people-contacts-relationship-graph) — 13 sources
   4. [Health: Wearables & Biometrics](#health-wearables-biometrics) — 21 sources
   5. [Health: Nutrition, Medical Records, Labs & Genetics](#health-nutrition-medical-records-labs-genetics) — 26 sources
   6. [Computer & Developer Activity](#computer-developer-activity) — 22 sources
   7. [Web Activity & Content Consumption](#web-activity-content-consumption) — 23 sources
   8. [Home, IoT & Smart Devices](#home-iot-smart-devices) — 25 sources
   9. [Environment & Ambient Context](#environment-ambient-context) — 25 sources
   10. [Geolocation & Travel](#geolocation-travel) — 23 sources
   11. [Calendar, Tasks, Habits & Productivity](#calendar-tasks-habits-productivity) — 29 sources
   12. [Artifacts: Notes, Documents, Drafts & Files](#artifacts-notes-documents-drafts-files) — 26 sources
   13. [Photos & Visual Media](#photos-visual-media) — 14 sources
   14. [Media: Music, Podcasts, Video & TV](#media-music-podcasts-video-tv) — 27 sources
   15. [Media: Books, Reading & Gaming](#media-books-reading-gaming) — 20 sources
   16. [Financial, Spending & Purchases](#financial-spending-purchases) — 31 sources
   17. [Social Media & Web Presence](#social-media-web-presence) — 24 sources

---

## Executive Summary

Trove has already absorbed the structurally hardest domains: the unified correspondence stream, the canonical financial transaction model with cross-source dedup (SimpleFIN + statement CSVs + Copilot), the health hub (Apple Health export + Oura), EventKit calendar/reminders, the activity watcher + cross-device Screen Time via Biome, and the M3 copy-then-read pattern with FDA already granted. This means the integration frontier is no longer about inventing mechanisms — it is about pointing a handful of already-proven mechanisms (M5 OAuth/keyed REST, M1 export importer, M3 local SQLite read, M2 watch folder) at a long tail of high-value sources. Across all 17 domains, the dominant pattern is overwhelmingly favorable: the majority of net-new sources are either keyless/token public APIs (Last.fm, Trakt, Steam, Chess.com, Lichess, BGG, Etherscan, Blockstream, NOAA/USGS/USNO families, Hue/Tempest LAN) or clean one-shot exports (Takeout family, GDPR archives, brokerage/P2P CSVs, genetics raw files). Very little in the entire research corpus is a genuine hard block.

The single highest-leverage observation is that a few aggregator hubs each collapse many underlying sources into one integration. Last.fm and ListenBrainz aggregate every scrobbling music player; Trakt and Simkl aggregate TV/film watch history across Plex/Infuse/streaming; Readwise aggregates Kindle + Apple Books + web highlights; Strava aggregates Garmin/Wahoo/Apple Watch workouts; macOS Contacts (CNContactStore) federates iCloud + Google + Exchange + CardDAV in one TCC prompt; Apple Health export already federates dozens of wearables for iPhone users; SnapTrade federates 30+ brokerages; and the Microsoft Graph API federates Outlook mail + calendar + Teams + To Do + OneNote + OneDrive. Prioritizing these hubs yields disproportionate coverage per unit of effort and should anchor the roadmap.

The second cross-cutting theme is time-sensitivity. Most of this domain is backfillable forever (public-API environmental data, blockchain, account exports that always contain full history), but a critical minority is lossy if not captured live: messaging-app DMs (Slack live DMs, Signal/Discord context), now-playing/scrobble streams, meeting transcripts on services with 30-day retention (Google Meet, some Krisp/tl;dv webhooks), Ambient Weather/Ring (1-year retention), Google Meet transcript API (30-day), and any service trending toward shutdown (Pocket/Omnivore already gone; Skype dead). These deserve priority weighting even where individually lower-value, because the window closes.

The third theme is reusable mechanism investment. Several infrastructure pieces each unlock a dozen-plus sources at once: a generic SMART-on-FHIR client (every US hospital/lab), a generic token-REST connector framework with a per-service config (dozens of task/finance/media APIs that differ only in endpoint shape), a generic ZIP/export importer that routes by detected structure (every GDPR archive + Takeout), an entity-resolution layer (turns every correspondence/contact/calendar source into a coherent person graph at zero new permission cost), a Vision-framework OCR bridge (screenshots + imported images + lab PDFs), a FIT-file parser (Garmin/Wahoo/Coros/Suunto all at once), and a cloud-drive folder watcher (iCloud/Dropbox/Google Drive/OneDrive are all just local folders post-File-Provider). Building these primitives first multiplies the value of every subsequent connector.

The genuinely hard blocks are few and consistent: Apple's own E2E-encrypted on-device stores (Significant Locations, Maps Visited Places, FinanceKit on desktop) are walls no third party can pass; a handful of services have closed APIs with no export (Kagi by design, Sunsama, 500px, LINE, WeChat); and the bank-aggregation alternatives (Plaid/Teller) plus brokerage aggregator (SnapTrade) are blocked by the standalone-distribution problem of developer-held keys — already correctly solved by SimpleFIN's user-owned-credentials model. Everything else is a matter of prioritization.

## Top Recommendations

Highest-value **net-new** sources to build next, weighing value × feasibility × time-sensitivity.

| Priority | Source | Domain | Effort | Why |
|---|---|---|---|---|
| **P0** | macOS Contacts (CNContactStore) + derived Interaction Graph + Entity Resolution | People | M | The anchor for the entire person layer. One TCC prompt federates iCloud/Google/Exchange/CardDAV contacts with birthdays/anniversaries. The interaction graph is then ZERO new permissions — pure read over correspondence/calendar/calls already in the vault — and the exact-match entity resolver (E.164 phone + lowercase email) makes every existing source coherent. Highest value-per-effort in the whole corpus. |
| **P0** | Last.fm + ListenBrainz | Media (Music) | S | Universal music aggregators: any player that scrobbles (Spotify, Apple Music, Tidal, Navidrome) flows here. Keyless read (Last.fm), clean JSON, full paginated history + incremental watermark poll. Pairs directly with the already-built Apple Music scrobbler. Shared poll pattern serves both. |
| **P0** | Trakt.tv (+ Simkl) | Media (TV/Film) | S | The Last.fm of TV/movies — aggregates watch history from Plex/Infuse/Emby. PKCE OAuth, free full history, ratings + watchlists. Time-sensitive: forward watch history is only captured if collecting now. Backfilled by Netflix/Letterboxd/IMDb CSV imports. |
| **P0** | Gmail + generic IMAP + Microsoft Graph (one mail connector) | Email | M | Mail is foundational personal data and feeds derived sources (flight parsing, receipts, newsletters, entity resolution). One async-imap/XOAUTH2 collector serves Gmail, iCloud, Fastmail, Outlook/M365 personal+work, custom domains. mbox M1 already exists for bulk; M5 adds live incremental (historyId/delta). Graph also unlocks calendar/Teams/To Do/OneNote downstream. |
| **P0** | Shell history + local git + Claude Code transcripts + GitHub | Developer Activity | S | All planned, all near-zero friction (plain files / gitoxide / PAT REST), all high-signal for 'what did I do/build today'. Complements the activity watcher perfectly. GitHub adds PRs/issues/stars that local git misses. Batch these as one developer-activity wave. |
| **P1** | Generic SMART-on-FHIR client (Epic + Quest + Labcorp + any EHR) | Health (Medical) | L | One Authorization-Code+PKCE FHIR R4 client covers a majority of US patients (Epic alone) plus structured labs (LOINC-coded) from Quest/Labcorp and any compliant EHR. This is the canonical structured-medical-record path and it is a single implementation, not per-provider. Register once on open.epic.com. |
| **P1** | Strava | Geolocation/Health | M | Aggregates GPS workouts from Garmin/Apple Watch/Wahoo/Polar/Suunto into one OAuth pull; webhook for near-real-time. GPS polylines, segments, power not in Apple Health. High overlap value across health + geo domains. M1 bulk export backfills history without quota. |
| **P1** | Things 3 + Toggl Track (local-DB task/time) | Productivity | S | Highest-feasibility productivity sources — readable SQLite, no network, app needn't be running, FDA already held. Things 3 is the dominant Mac task manager; Toggl is a top time tracker (dual-path local DB + API). Same M3 pattern already used for iMessage/Books. |
| **P1** | Google Calendar + Todoist + Linear + Jira + Asana (token-REST task/cal batch) | Productivity | M | All clean personal-token or standard-OAuth REST with completed-task history. Pairs with the built EventKit calendar and TickTick. Build as one generic token-REST connector framework parameterized per service — marginal cost per added service is tiny. |
| **P1** | Apple Photos (Photos.sqlite + psi.sqlite ML labels + EXIF) | Photos | M | Geotags are the single best location-history proxy Trove can get on macOS (Significant Locations is encrypted-blocked). Faces/people + Apple's free pre-computed ML scene labels are unique signals. Metadata-only read via the existing copy-then-read pattern; never duplicate images. |
| **P1** | Steam + Chess.com + Lichess + BoardGameGeek | Gaming | S | All keyless/public REST, identical implementation pattern, each a unique data source with no alternative path. Steam is the gold-standard gaming API. Low effort, broadly applicable, fully standalone-safe. |
| **P1** | Granola + Fathom + Zoom + Fireflies (meeting transcripts) | Meetings | M | Granola/Fathom already have MCP scaffolding noted; all four are clean M5 REST/GraphQL into one meetings/<service>/YYYY-MM.jsonl sink. Zoom is the dominant work-meeting platform. Time-sensitive: some transcripts have retention limits, so live capture matters. |
| **P1** | Readwise + Readwise Reader | Web/Reading | S | Richest highlights/read-later API; single integration aggregates Kindle, Apple Books, web articles, PDFs, emails. Token-based, incremental via updatedAfter. The high-leverage reading hub. |
| **P1** | Brokerage + crypto batch (Schwab API, IBKR Flex, Coinbase/Kraken, Etherscan, Blockstream + Fidelity/Vanguard/Robinhood CSV) | Financial | M | The clear next financial tier on top of the built transaction model. Schwab/IBKR are official + individual-developer-friendly; Etherscan/Blockstream are keyless public chain reads (user supplies address, never keys); brokerage CSVs are trivial M1. Investments are a major net-new financial dimension. |
| **P1** | Cloud-drive folder watcher (iCloud Drive + Dropbox + Google Drive + OneDrive) | Artifacts | S | Post-File-Provider these are all just local folders under ~/Library/CloudStorage — one watch/import mechanism covers four services. Skip undownloaded .icloud placeholders gracefully. Feeds the artifacts importer at near-zero marginal cost per provider. |
| **P1** | GDPR/Takeout export importer batch (YouTube watch history, Reddit, Discord, Facebook/Instagram/Threads, Telegram, X archive) | Social/Messaging | M | One ZIP-routing importer absorbs nearly every social + messaging archive (Meta JSON, Takeout JSON, Telegram JSON, Discord package, X archive, Reddit GDPR). High personal value, fully standalone, and for closed platforms (X/IG/Reddit) the export is the ONLY viable path since APIs are priced/gated out. |
| **P1** | Apple Notes + Bear + Drafts + Day One + Obsidian/Logseq | Artifacts | M | Obsidian/Logseq/Drafts/Bear are planned and trivial (plain files / documented SQLite). Day One JSON export is clean. Apple Notes (gzipped-protobuf) is M effort but a one-time migration importer. Core authored-knowledge layer; FDA already held. |
| **P1** | WHOOP + Withings + Fitbit/Google Health + Dexcom (vendor wearable APIs) | Health (Wearables) | M | These carry vendor-exclusive fields NOT in Apple Health: WHOOP strain/recovery, Withings body-composition + BP + sleep-mat, Dexcom continuous glucose, Fitbit years of pre-HealthKit history. All self-service OAuth. Highest-value beyond the Apple Health hub. |
| **P1** | Keyless environmental batch (Open-Meteo Air Quality + USGS earthquakes + NWS alerts + USNO sun/moon + SWPC aurora + AirNow/WAQI AQI + NASA FIRMS wildfire) | Environment | S | All keyless public APIs sharing the existing Open-Meteo HTTP client; each is a thin daily/hourly poll into environment/. Backfillable forever so low risk, but high ambient-context value and trivial incremental effort each. Air Quality is already planned. |
| **P1** | Apple Voice Memos + Visual Voicemail + Google Voice | Calls/Voice | S | Voice Memos is planned; macOS 15+ native transcripts make it zero-dependency (pure tsrp atom parse). Google Voice Takeout gives voicemail transcripts + call logs. These capture spoken personal content otherwise lost. |
| **P2** | Genetics raw import (23andMe + AncestryDNA, one parser) | Health (Genetics) | S | Same 4-column TSV for both; covers the vast majority of consumer genetic testing. Very high personal value, one-time M1, and a privacy differentiator (local-only ClinVar/SNPedia annotation is a compelling follow-on that never leaves the machine). |
| **P2** | Nutrition CSV batch (Cronometer + MyFitnessPal + MacroFactor) | Health (Nutrition) | S | All clean CSV exports, one importer with per-app column maps. Cronometer is the micronutrient gold standard. Complements the Apple Health nutrition passthrough with per-meal detail. |
| **P2** | Bluesky + Mastodon (open-protocol social) | Social | M | Best-in-class open APIs: Bluesky CAR export is unauthenticated and complete; Mastodon is fully open. Both support clean incremental M5 pull — the rare social platforms where live sync is actually feasible and free. |
| **P2** | Local-DB media: Shazam + Flighty + NetNewsWire + Overcast | Media/Travel/Web | S | Shazam (music discovery, iCloud-synced to Mac) and Flighty (flight history) are zero-auth M3 SQLite reads; NetNewsWire (RSS, open-source schema) M3; Overcast the best podcast-history source via extended OPML. Each unique, all low effort on the established pattern. |
| **P2** | Home/IoT local-first batch (HomeKit homed DB + Hue LAN + WeatherFlow Tempest + Enphase solar + Green Button utility) | Home/IoT | M | Local reads / LAN polls / keyless specs with no cloud dependency — best standalone fit in the IoT domain. HomeKit captures whole-home topology; Enphase/Green Button capture energy with no equivalent source. One smart-home JSONL-per-device schema covers all. |
| **P2** | macOS Download history (QuarantineEventsV2) + Safari Reading List | Web/Files | S | Both zero-permission-beyond-FDA M3 reads with rich signal: download source URLs persist after file deletion; Reading List complements the already-built Safari history. Cheap wins that round out web-activity coverage. |
| **P2** | Books/reading export batch (Goodreads + StoryGraph + Kindle My Clippings + Apple Books local DB) | Media (Books) | S | Goodreads/StoryGraph CSV + Kindle My Clippings.txt + Apple Books SQLite are all S-effort on existing patterns; Readwise covers the highlight overlap but these are the zero-dependency fallbacks for non-Readwise users. |

## Reusable Mechanism Investments

Invest in these primitives first — each unlocks a dozen-plus sources:

1. Generic token-REST connector framework (M5). Most net-new cloud sources differ only in base URL, auth header shape, and response mapping — Todoist, Linear, Jira, Asana, Trello, Clockify, Harvest, Habitica, YNAB, Lunch Money, Fireflies, Granola, Fathom, Read.ai, Raindrop, Pinboard, Hypothesis, Instapaper, Monica, GroupMe, Hardcover, Steam, Chess.com, Lichess, BGG, RetroAchievements. Build one config-driven connector (endpoint + auth + pagination + JSONL sink + watermark) and adding a service becomes a small config + mapper. Extends the existing sync/oauth.rs + ticktick.rs/oura.rs pattern.

2. Generic export/ZIP importer with structure routing (M1). One importer that detects archive shape and routes: Takeout (YouTube/Keep/Maps/Chat), Meta family (Facebook/Instagram/Threads/Messenger — shared JSON parser), Telegram JSON, Discord package, X archive, Reddit GDPR, brokerage/P2P CSVs, genetics TSV, nutrition CSVs, EML folders. Already partly exists for mbox/Slack — generalize the dispatcher.

3. Generic SMART-on-FHIR R4 client (M5). Single Authorization-Code+PKCE client + capability discovery covers Epic (majority of US patients), Quest, Labcorp, CMS Blue Button, and any compliant EHR. Per-provider work collapses to a server-URL selection step. Target resources: Patient, Condition, Observation (labs+vitals), MedicationRequest, Immunization, AllergyIntolerance, DiagnosticReport, Procedure, DocumentReference.

4. Entity-resolution layer (M6, pure local compute). Normalize phone to E.164, email to lowercase-trim, map handles to one CNContact. Turns every correspondence/calendar/call/contact source into a coherent person graph at zero new permission. Ship exact-match first; defer fuzzy name matching.

5. Apple Vision OCR/document bridge (objc2-vision or Swift helper, like the EventKit bridge). Shared infra serving screenshot indexing, imported-image OCR, and lab/dental/signed-PDF text extraction. One bridge, many consumers.

6. FIT-file parser (fitparser crate, compiled in). Decodes Garmin, Wahoo, Coros, Suunto activity files in one path — GPS + power + HR + training metrics not in Apple Health.

7. Cloud-drive folder watcher (M2/M3). iCloud Drive, Dropbox, Google Drive, OneDrive are all local folders under ~/Library/CloudStorage post-File-Provider — one watch mechanism, four services, handle .icloud placeholders gracefully.

8. CalDAV/CardDAV + vCard/ICS parsers. Generic vCard importer covers iCloud/Google/Outlook contact exports at once; CalDAV catches any non-Google/non-Microsoft calendar; ICS parsing also backstops TripIt/flight itineraries.

9. Local-bundled annotation datasets (ClinVar/SNPedia VCF snapshot). Enables offline genetics variant analysis with no data leaving the machine — a privacy differentiator and the model for other compiled-in reference data.

## Aggregator Hubs

Aggregators that each capture many underlying sources — prioritize these:

- macOS Contacts (CNContactStore): federates iCloud + Google + Exchange + LDAP + CardDAV contacts in one TCC prompt, with built-in cross-account dedup. The anchor of the person layer.
- Apple Health export.zip (already built): federates Garmin/Fitbit/Polar/Withings/Omron/Amazfit/Renpho and dozens more for any iPhone user — already covers a large fraction of the wearables domain. Deepen its parser (routes/ECG/CDA/nutrition/audio-exposure) rather than building those vendor APIs first.
- Last.fm + ListenBrainz: every scrobbling music player (Spotify, Apple Music, Tidal, Navidrome) in one keyless pull.
- Trakt.tv + Simkl: TV/film watch history across Plex/Infuse/Emby/streaming in one OAuth pull.
- Readwise / Reader: Kindle + Apple Books + web highlights + PDFs + article saves in one token API.
- Strava: GPS workouts from Garmin/Apple Watch/Wahoo/Polar/Suunto in one OAuth pull (multi-device athletes' full history).
- Microsoft Graph: Outlook mail + calendar + Teams transcripts + To Do + OneNote + OneDrive — one Azure AD OAuth scaffold unlocks the entire Microsoft ecosystem.
- Google (OAuth worktree in progress): Gmail + Calendar + Contacts (People API) + Drive + Tasks + Meet + Keep/Takeout — one consent bundle, many connectors. Already partially built.
- SnapTrade: 30+ brokerages via one API (BUT standalone-key-blocked; viable only BYOK or relay — keep as spike, prefer official Schwab/IBKR + CSVs).
- Beeper: many messaging networks via one local API (BUT requires Beeper running — M6 opt-in only, violates strict standalone).
- Home Assistant: all home devices in one pull IF the user already self-hosts (opportunistic poll, never a dependency).
- Nightscout: many CGM sources (Dexcom/Libre/Medtronic) for the self-hosting T1D community.

## Hard Blocks

Genuinely blocked — do not revisit unless the noted condition changes:

- Apple Significant Locations & Maps Visited Places (macOS/iOS): AES-encrypted with Secure-Enclave keys; FDA opens the file but content is unreadable. No third-party path. Geotags from Apple Photos are the location-history proxy instead. Monitor for a GDPR-motivated official export.
- FinanceKit / Apple Card-Cash-Savings live sync: iOS-only entitlement; no Mac API. Wallet/card.apple.com CSV is the only desktop route (already importable). A companion iOS app is the only live path — large scope, iceboxed.
- Plaid / Teller / SnapTrade for standalone distribution: developer-held keys can't safely ship in a distributed binary and a relay violates local-first. SimpleFIN's user-owned-credential model already solves bank aggregation correctly; these stay icebox / BYOK-only.
- Kagi search history: no server-side history by design (privacy feature). Unsolvable.
- WeChat (macOS): key extraction needs a running process + lldb memory scan (violates standalone); primary tooling discontinued Oct 2025; ToS prohibits. Icebox.
- LINE (macOS): undocumented local DB, no export, no personal API. Icebox until community tooling emerges.
- 500px: no API, no export — only scraping (violates standalone/ToS). Skip.
- macOS CoreMotion barometer / CMAltimeter: not exposed on macOS regardless of hardware. Use Open-Meteo pressure_msl instead.
- Blitzortung lightning: usage policy forbids direct client connections for a local-first app. NWS alerts cover thunderstorm warnings.
- FSEvents / SFL2 recent-files raw parse: root-required / NSKeyedArchiver opaque blobs, massive noise. Use Spotlight (kMDItemLastUsedDate) metadata queries instead.
- Pocket / Omnivore / Skype / Pandora: services dead or API-closed — historical M1 import only, no live path.
- Krisp / tl;dv / Read.ai webhook-only delivery: requires a public HTTPS receiver (violates local-first) unless their MCP runs locally; fall back to M1 export or polling where available.
- Beeper / Home Assistant / Timing-app AppleScript: not hard-blocked but violate strict standalone (require the app running) — acceptable only as explicitly-opted-in M6 collectors, never as a Trove dependency.

## Cross-Cutting Themes

Cross-cutting themes:

TIME-SENSITIVE / UNRECOVERABLE — prioritize live capture even when individually lower-value: now-playing/scrobble streams (Last.fm, MediaRemote, the built Biome streams); messaging DMs that exports miss (Slack live DMs/private channels, Signal context, Discord received-message context); meeting transcripts on retention-limited services (Google Meet 30-day API window, some webhook-only recorders); short-retention device clouds (Ambient Weather and Ring ~1-year, Garmin/COROS recent windows); forward watch/listen history on aggregators (Trakt/Last.fm only capture from when you start collecting). By contrast, almost everything keyless-public (NOAA/USGS/USNO/Open-Meteo archives back to 1940, blockchain, account GDPR exports) is backfillable forever and carries low urgency — sequence these by value, not urgency.

PRIVACY-SENSITIVE — vault-isolate and opt-in with explicit acknowledgement: dating apps (Tinder/Hinge/Bumble — swipe + message + match content), health/medical/genetics (FHIR records, CGM, 23andMe variants), location trails, message bodies, financial detail, ambient microphone dB sampling (never store audio, only RMS), Apple voice/voicemail transcripts. Password-manager imports must hard-strip secrets at parse time (metadata only). The local-only ClinVar annotation story is a genuine privacy differentiator worth marketing.

STANDALONE-LINE JUDGMENT CALLS: reading another app's SQLite/export on disk = OK (Things 3, Toggl, Flighty, Plex, Apple Notes, RSS readers) because the data exists regardless of whether the app runs. Requiring an app running at collection time = NOT OK as a default (Beeper, Home Assistant, Timing AppleScript, Krisp/tl;dv cloud webhooks, Eight Sleep rooted-Pod) — these are acceptable only as clearly-labeled opt-in M6 collectors for users who already run them. Unofficial/reverse-engineered cloud APIs (PSN, Xbox via xbl.io, Ring, Audible, Eight Sleep, Emporia) pass the standalone test (they're HTTP calls, not running-app deps) but carry breakage risk — build with graceful failure and clear 'unofficial' disclosure.

ENTITY-RESOLUTION is the connective tissue: Trove already holds iMessage handles, email addresses, calendar attendees, call numbers, and (next) contacts — a single normalization pass makes them one person graph at zero new permission. This is the highest-leverage non-collector investment and should ship with the first contacts collector. It also makes downstream features (relationship intelligence, per-person timelines, dedup across social archives) possible.

DEEPEN-BEFORE-BROADEN on already-built hubs: the Apple Health export parser should be extended (workout-route GPX, ECG CSV, clinical CDA, nutrition, audio-exposure, mindfulness) before building individual vendor wearable APIs, since the export already federates most wearables for iPhone users — better marginal value than a new OAuth connector. Similarly, the existing media.rs/podcasts.rs/books.rs/calls.rs collectors (present in the tree beyond the brief's list) suggest some 'planned' items are partly done — audit those before rebuilding.

STANDALONE-LINE on the built finance model is the template to replicate: SimpleFIN's user-owned-credential approach is exactly how to handle every aggregator that would otherwise be key-blocked. Apply the same stance to brokerage (prefer official Schwab/IBKR self-service + CSV over SnapTrade) and EU banking (GoCardless BYOK over a relay).

---

# Domain Catalogs

## Email & Messaging Apps

This domain spans every written-conversation channel from email providers and messaging apps to enterprise chat platforms. The good news for Trove: the highest-value sources (Gmail, IMAP-accessible email, Apple Mail local store, iMessage — already built, Slack export — already built, Telegram) all have concrete, privacy-respecting access paths. The harder sources (Signal, WhatsApp, WeChat) require reading SQLCipher-encrypted local DBs whose keys have become progressively harder to extract without running the app. Enterprise channels (Teams, Google Chat) work fine for org accounts but are blocked or awkward for personal/consumer accounts. Trove already has mbox import, Slack workspace-export import, and iMessage built; the next highest-leverage additions are Gmail/IMAP M5 pull, Apple Mail local DB (M3), Telegram Desktop export (M1), Discord data package (M1), Outlook/Microsoft 365 Graph (M5), and ProtonMail via the official export tool or Bridge (M1/M5).

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Gmail | Email | M5; M1 fallback | OAuth (Google, bring-your-own or compiled-in app credentials) | M | 🟢 High — Gmail API is fully alive in 2026, well-documented, supports incremental sync via historyId, Rust HTTP is trivial. Takeout mbox already importable via existing email.rs. | 📋 planned |
| Generic IMAP (any provider) | Email | M5 | OAuth or app-password credential; none beyond network | M | 🟢 High — any IMAP-capable mailbox (iCloud Mail, Fastmail, Yahoo, Zoho, self-hosted, etc.) works. XOAUTH2 covers Gmail and Outlook; app-specific passwords cover iCloud. async-imap gives a clean async Rust API. | 📋 planned |
| Apple Mail (local store) | Email | M3 | Full Disk Access (FDA) — ~/Library/Mail is protected | M | 🟢 High — SQLite schema is well-documented and stable across V9/V10. Envelope index gives headers and snippets without touching .emlx; full body requires the .emlx files. FDA already needed for iMessage (chat.db) so the permission is already on the ladder. | 🆕 new |
| Fastmail (JMAP) | Email | M5 | API token (account setting, no OAuth dance) | S | 🟢 High — JMAP is strictly better than IMAP for sync; Fastmail's implementation is production-quality and actively developed. Token auth is simpler than OAuth. | 🆕 new |
| Email .mbox import | Email | M1 | none | S | 🟢 High — already built. | ✅ built |
| Slack workspace export import | Team Chat | M1 | none | S | 🟢 High — already built. | ✅ built |
| Slack API pull (live DMs + private channels) | Team Chat | M5 | OAuth (Slack app registration or bring-your-own token) | M | 🟢 High — Slack API is stable and well-documented. User tokens do not expire by default. Covers DMs even on free workspaces, which the workspace export does not. | 🆕 new |
| Telegram (Desktop export — M1) | Messaging | M1 | none | M | 🟢 High — official built-in feature, stable JSON schema, no authentication complexity for Trove. JSON output includes text, media references, forwarded-from, reactions. | 🆕 new |
| Telegram (MTProto API — incremental pull) | Messaging | M5 | API credentials (api_id/api_hash from my.telegram.org — free but not keyless) | L | 🟡 Medium — MTProto user API is powerful but complex (custom binary protocol); Telegram ToS prohibits mass automation but personal archiving of own messages is OK; account can be banned if requests look like scraping. grammers crate exists but is less mature than Telethon. | 🆕 new |
| iMessage / SMS | Messaging | M3 | Full Disk Access (FDA) | S | 🟢 High — already built. | ✅ built |
| Discord (data package import) | Gaming / Community Chat | M1 | none | M | 🟢 High — official mechanism, well-documented JSON schema, contains all DMs and server messages the user sent (note: only sent messages, not messages received in context — no full thread history). | 🆕 new |
| Microsoft Outlook / Microsoft 365 (Graph API) | Email | M5 | OAuth (Microsoft Entra app registration or bring-your-own client_id) | M | 🟢 High — Graph API is stable, supports personal accounts, EWS fully deprecated October 2026 (Graph is now the only path). Delta query enables efficient incremental sync. | 🆕 new |
| Outlook for Mac (local OLM store) | Email | M3 | Full Disk Access (FDA) | L | 🟠 Low — OLM/.olk15 is a proprietary Microsoft format with no public spec; parsing requires reverse engineering. The Graph API M5 path covers the same data cleanly for any account that is still online. The OLM path only adds value for fully offline/archived accounts. | 🆕 new |
| ProtonMail | Email | M1; M5 fallback via Bridge + IMAP | Proton account credentials; Bridge requires paid Proton plan | M | 🟢 High — official export tool is open-source, produces standard EML files that existing email.rs already handles (or trivially extends). Bridge path would violate standalone rule (requires Bridge running) so export tool is the right v1. | 🆕 new |
| Google Chat (Takeout export) | Team Chat | M1 | none | M | 🟢 High — official Takeout mechanism, stable JSON format. | 🆕 new |
| Signal Desktop | Messaging | M3 | Full Disk Access (FDA) + Keychain access for the encryption key | L | 🟡 Medium — DB is on disk and readable in principle, but the key migration to Keychain (completed mid-2024) means Trove must request the key from the macOS Keychain under Signal's service name, which requires the user to approve keychain access per-app. The sqlcipher Rust crate can then decrypt the DB. Schema is documented by community (Signal-Desktop GitHub). No official API or export exists. | 🆕 new |
| WhatsApp (Mac desktop) | Messaging | M1 | none (export); FDA (local cache, limited value) | M | 🟡 Medium — No complete local DB on the Mac app. The practical path for WhatsApp history is: (1) In-app Export: WhatsApp iOS/Mac → individual chat → More → Export Chat → .txt or .zip with media. This gives a text file per chat, parseable but lossy. (2) iPhone backup: the ChatStorage.sqlite in an unencrypted iPhone backup (~/Library/Application Support/MobileSync/Backup/<UUID>) is fully readable — but this overlaps with other Apple backup integrations. | 🆕 new |
| Facebook Messenger | Messaging | M1 | none (user-initiated export) | M | 🟢 High — official export, well-structured JSON, covers all DMs and group chats. No API equivalent for personal accounts (Graph API for Messenger is restricted to businesses/platforms). | 🆕 new |
| Instagram DMs | Social / Messaging | M1 | none | S | 🟢 High — same JSON structure as Facebook Messenger export; can likely share the same parser. | 🆕 new |
| Microsoft Teams (work/school accounts) | Team Chat | M5; M1 fallback | OAuth (Microsoft Entra app registration); work/school account required for full Graph Teams access | M | 🟡 Medium — Graph API Teams access works well for Microsoft 365 work/school accounts. Personal consumer Microsoft accounts have significantly restricted Teams API access (Group.* permissions not supported for personal accounts). Teams Export API billing was eliminated August 2025. | 🆕 new |
| X (Twitter) DMs | Social / Messaging | M1 | none | M | 🟢 High — official archive mechanism includes DMs. JSON structure is parseable once the JS wrapper is stripped. Note the X API v2 DM endpoints exist (Chat.Read scope) but require paid developer account tier for meaningful access. | 🆕 new |
| GroupMe | Messaging | M5 | API token (from dev.groupme.com — no app registration needed) | M | 🟢 High — simple REST API, personal token, no complex OAuth. GroupMe is popular in US universities and sports teams. | 🆕 new |
| WeChat (macOS desktop) | Messaging | M3 | Full Disk Access (FDA) + running WeChat process for key extraction | XL | 🟠 Low — key extraction requires a running WeChat process and lldb memory scanning, violating the standalone rule. The Chatlog CLI tool was discontinued in October 2025 due to WeChat policy compliance issues. No official export API or data export feature. WeChat Terms of Service prohibit third-party data extraction. | 🆕 new |
| LINE (macOS desktop) | Messaging | M3 | FDA (sandboxed container path) | L | 🟠 Low — no public documentation of the local database format or path. LINE is sandboxed so the DB path is only accessible with FDA. No official export/API for personal messages. LINE's API is business-oriented (LINE Developers, Messaging API for bots only). Local DB format is unknown/undocumented. | 🆕 new |
| Beeper (unified messaging) | Messaging Aggregator | M3 | none (localhost API, no TCC) | M | 🟡 Medium — Beeper Desktop API is documented and local-only, which is great. However, it requires Beeper to be installed AND running (standalone rule violation). Beeper is a paid app ($X/month). Bridging varies — some networks (WhatsApp, Signal) use on-device connections; others may relay through Beeper's servers. | 🆕 new |
| Matrix / Element | Messaging | M5 | Matrix access token (from Element/client login); no macOS TCC needed | M | 🟢 High — Matrix Client-Server API is a published open spec (spec.matrix.org), the sync endpoint is the standard incremental fetch, and access tokens are long-lived. Works for any homeserver (matrix.org, self-hosted, etc.). | 🆕 new |
| Discord (DiscordChatExporter JSON import) | Gaming / Community Chat | M1 | none (Trove side) | S | 🟢 High — DiscordChatExporter is widely used and produces clean JSON. Trove only needs to parse the output format; the user runs the tool themselves. | 🆕 new |
| IRC (ZNC / WeeChat logs) | Messaging | M3 | none (home directory paths) | S | 🟢 High — pure plaintext log files, no encryption, no permissions beyond home directory access. Niche but trivial to implement. | 🆕 new |
| ProtonMail (native export tool — EML/JSON) | Email | M1 | Proton account credentials (entered by user into the export tool) | S | 🟢 High — official tool, standard EML output, feeds directly into existing mbox/EML import. Works on free plans (no paid requirement unlike Bridge). | 🆕 new |

### Detail

#### Gmail — _Email_

🟢 **High — Gmail API is fully alive in 2026, well-documented, supports incremental sync via historyId, Rust HTTP is trivial. Takeout mbox already importable via existing email.rs.** · M5; M1 fallback · OAuth (Google, bring-your-own or compiled-in app credentials) · effort **M** · 📋 planned

- **Access:** REST API: https://gmail.googleapis.com/gmail/v1/users/me/messages — list + get with full RFC822 payload. OAuth 2.0 scope gmail.readonly. Also: Google Takeout → mail.google.com → Download your data → Mail → .mbox per label.
- **Recommendation:** Build now — highest-value email source; mbox M1 path already works for bulk history, M5 pull adds live incremental sync. Use async-imap or direct REST.
- **Notes:** gmail.readonly scope grants read-only access to all messages, threads, labels. historyId enables efficient incremental pulls. Takeout exports as one .mbox per label; existing email.rs already handles that. Rate limit: 250 quota units/second per user — fine for personal use. Google OAuth requires app registration but a bring-your-own client_id fallback satisfies the 'built for anyone' rule.

#### Generic IMAP (any provider) — _Email_

🟢 **High — any IMAP-capable mailbox (iCloud Mail, Fastmail, Yahoo, Zoho, self-hosted, etc.) works. XOAUTH2 covers Gmail and Outlook; app-specific passwords cover iCloud. async-imap gives a clean async Rust API.** · M5 · OAuth or app-password credential; none beyond network · effort **M** · 📋 planned

- **Access:** RFC 3501 IMAP4rev1, port 993 TLS. XOAUTH2 SASL for Gmail/Outlook; app-password for others. Rust: async-imap crate (actively maintained, chatmail org). FETCH RFC822 or BODY.PEEK[].
- **Recommendation:** Build now alongside Gmail — share the same collector; IMAP covers iCloud Mail, Fastmail, ProtonMail (via Bridge), and any custom domain. One generic collector serves all providers.
- **Notes:** iCloud Mail IMAP: host imap.mail.me.com port 993, requires app-specific password (Apple ID → security → app passwords). Fastmail supports JMAP (RFC 8620) as well as IMAP — JMAP is strictly superior (single round-trip for full sync state, JSON body) and Fastmail exposes it via an API token; worth a dedicated JMAP path. Microsoft IMAP: imap-mail.outlook.com port 993, XOAUTH2 required (EWS deprecated October 2026, Basic Auth already dead). ProtonMail IMAP: requires Bridge app running locally — this violates the 'no runtime dependency' rule for live pull, so the native Proton export tool (EML/MBOX) is the better v1 path.

#### Apple Mail (local store) — _Email_

🟢 **High — SQLite schema is well-documented and stable across V9/V10. Envelope index gives headers and snippets without touching .emlx; full body requires the .emlx files. FDA already needed for iMessage (chat.db) so the permission is already on the ladder.** · M3 · Full Disk Access (FDA) — ~/Library/Mail is protected · effort **M** · 🆕 new

- **Access:** Envelope Index SQLite at ~/Library/Mail/V10/MailData/Envelope Index (macOS Sequoia/Sonoma/Ventura = V10; Monterey = V9). Key tables: messages, subjects, addresses, recipients, attachments. Full bodies in .emlx files at ~/Library/Mail/V10/<UUID>/<Mailbox>.mbox/Messages/<id>.emlx.
- **Recommendation:** Build now — zero-config for any Apple Mail user; no credentials required; complements M5 IMAP pull by capturing locally cached messages from any synced account (Gmail, Outlook, iCloud all appear here).
- **Notes:** V10 = macOS 13, 14, 15, 26. Copy DB before opening (SQLite WAL). The Envelope Index has headers + a body snippet (~first few hundred chars) in the summaries table; for full body, walk the corresponding .emlx file. Message-ID header in .emlx acts as dedup guid (same as mbox import). Both reads can use the existing correspondence/email/ JSONL sink. Guard against Mail.app having the DB locked — PRAGMA journal_mode=WAL allows concurrent reads safely.

#### Fastmail (JMAP) — _Email_

🟢 **High — JMAP is strictly better than IMAP for sync; Fastmail's implementation is production-quality and actively developed. Token auth is simpler than OAuth.** · M5 · API token (account setting, no OAuth dance) · effort **S** · 🆕 new

- **Access:** JMAP core + mail (RFC 8620 + RFC 8621). Endpoint: https://api.fastmail.com/jmap/session (session discovery). Auth: API token (Settings → Privacy & Security → API tokens). Single token grants access to email, contacts, and calendar. Supports Email/get, Email/query, Email/changes for efficient incremental sync.
- **Recommendation:** Build now as a JMAP-specific path if the generic IMAP collector is built; adds minimal marginal effort and greatly improves Fastmail sync efficiency. Fallback to IMAP for non-JMAP Fastmail.
- **Notes:** JMAP Email/changes gives an RFC-standard incremental sync cursor — far more efficient than IMAP SINCE searches. Fastmail is a strong privacy-aligned provider popular among Trove's likely user base. No Rust JMAP crate is battle-hardened as of 2026; likely needs a small HTTP client wrapper around reqwest — but the protocol is pure JSON so straightforward.

#### Email .mbox import — _Email_

🟢 **High — already built.** · M1 · none · effort **S** · ✅ built

- **Access:** User drops .mbox file; parse with existing email.rs using mail-parser crate. Sources: Google Takeout (mail/), Apple Mail File > Export Mailbox, Thunderbird, ProtonMail export tool (EML or MBOX choice), any RFC 4155-compliant export.
- **Recommendation:** Already built — extend to handle EML (individual files) as a variant if not already done.
- **Notes:** Covers Google Takeout mbox, Apple Mail mailbox exports, ProtonMail export-tool output, Thunderbird exports. Re-runnable with guid dedup. Full body stored. ProtonMail's open-source export tool (github.com/ProtonMail/proton-mail-export) produces EML/MBOX and is the preferred path for ProtonMail since Bridge requires the app running.

#### Slack workspace export import — _Team Chat_

🟢 **High — already built.** · M1 · none · effort **S** · ✅ built

- **Access:** Workspace Settings → Import/Export → Export → download ZIP. JSON per channel+date. Already built in slack.rs.
- **Recommendation:** Already built. Note: DMs and private channels require Business+ plan export or admin-level Export API. Standard free/Pro exports only include public channels. Document this limitation in UI.
- **Notes:** For a more complete Slack pull (DMs included): Slack user token (xoxp-) + conversations.history API can pull DMs the user was party to, without admin/Business+ requirement. This M5 complement is worth adding as 'Slack API pull' alongside the export import.

#### Slack API pull (live DMs + private channels) — _Team Chat_

🟢 **High — Slack API is stable and well-documented. User tokens do not expire by default. Covers DMs even on free workspaces, which the workspace export does not.** · M5 · OAuth (Slack app registration or bring-your-own token) · effort **M** · 🆕 new

- **Access:** OAuth 2.0 user token (xoxp-). Methods: conversations.list (enumerate channels/DMs), conversations.history (fetch messages, paginate with cursor), users.info (resolve IDs). App registered at api.slack.com with scopes: channels:history, im:history, mpim:history, groups:history, channels:read, im:read, users:read.
- **Recommendation:** Build now — fills the DM gap in the existing export import; same JSONL correspondence sink.
- **Notes:** Workspace export covers public channel history only unless Business+ admin. The user token M5 pull covers DMs and private channels the user is a member of, regardless of plan. Rate limit: Tier 3 = 50 req/min per workspace. Incremental via ts cursor — store watermark per channel in .trove/sync/slack-watermarks.json.

#### Telegram (Desktop export — M1) — _Messaging_

🟢 **High — official built-in feature, stable JSON schema, no authentication complexity for Trove. JSON output includes text, media references, forwarded-from, reactions.** · M1 · none · effort **M** · 🆕 new

- **Access:** Telegram Desktop: Settings → Advanced → Export Telegram Data → select chats → JSON or HTML. Output: a result.json (or results.json) with all messages, or per-chat HTML with media. Official schema documented at core.telegram.org/import-export.
- **Recommendation:** Build now — large user base, full chat history available via official export, clean JSON schema.
- **Notes:** Export is user-initiated but re-runnable. JSON schema: top-level 'chats' array, each chat has 'messages' array with id, date, from, text (array of text parts), media info. Media files exported alongside. Dedup by message id. A companion M5 path via Telegram's MTProto user API (grammers Rust crate at gramme.rs) enables live incremental pull but requires an api_id/api_hash from my.telegram.org — not keyless, but bring-your-own is viable.

#### Telegram (MTProto API — incremental pull) — _Messaging_

🟡 **Medium — MTProto user API is powerful but complex (custom binary protocol); Telegram ToS prohibits mass automation but personal archiving of own messages is OK; account can be banned if requests look like scraping. grammers crate exists but is less mature than Telethon.** · M5 · API credentials (api_id/api_hash from my.telegram.org — free but not keyless) · effort **L** · 🆕 new

- **Access:** MTProto API via grammers Rust crate (gramme.rs). Requires api_id + api_hash from my.telegram.org (free, user-registered). Flow: authenticate as a user (phone + 2FA), then messages.getHistory per dialog, incremental via offset_id.
- **Recommendation:** Spike first — validate grammers crate maturity and auth flow before committing. M1 export covers most users adequately; this is the incremental-sync upgrade.
- **Notes:** Telegram's Takeout API (core.telegram.org/api/takeout) provides a structured export mechanism via MTProto — safer than raw messages.getHistory for mass pull. Not to be confused with the Bot API (bots cannot read user messages). Compiled-in app api_id is not permitted by Telegram ToS; each user must register their own — document this clearly in UI.

#### iMessage / SMS — _Messaging_

🟢 **High — already built.** · M3 · Full Disk Access (FDA) · effort **S** · ✅ built

- **Access:** ~/Library/Messages/chat.db — SQLite, 15-min poll. Already built in imessage.rs.
- **Recommendation:** Already built.
- **Notes:** Covers iMessage, SMS, and RCS (iOS 18+). Group threads, reactions, tapbacks all in chat.db. Full Disk Access required.

#### Discord (data package import) — _Gaming / Community Chat_

🟢 **High — official mechanism, well-documented JSON schema, contains all DMs and server messages the user sent (note: only sent messages, not messages received in context — no full thread history).** · M1 · none · effort **M** · 🆕 new

- **Access:** Discord Settings → Privacy & Safety → Request All My Data → ZIP download (takes up to 30 days, usually hours). ZIP contains: messages/ folder, one subfolder per channel/DM (named by channel ID), each with messages.json array + channel.json metadata. Also account activity JSON, connected accounts, etc.
- **Recommendation:** Build now — Discord is among the highest-usage chat platforms; data package is the only sanctioned access path for personal message history.
- **Notes:** Critical limitation: the data package includes only messages the requesting user SENT, not the full thread context. Received messages in the same conversation are absent. This is a Discord policy decision. Third-party tools like DiscordChatExporter use user tokens (self-botting) to fetch full channel history — this violates Discord ToS and is not appropriate for Trove to automate. For full context, users can run DiscordChatExporter manually and drop the HTML/JSON output. Discord has no official API for personal DM export beyond the data package.

#### Microsoft Outlook / Microsoft 365 (Graph API) — _Email_

🟢 **High — Graph API is stable, supports personal accounts, EWS fully deprecated October 2026 (Graph is now the only path). Delta query enables efficient incremental sync.** · M5 · OAuth (Microsoft Entra app registration or bring-your-own client_id) · effort **M** · 🆕 new

- **Access:** Microsoft Graph API v1.0: GET /me/messages, /me/mailFolders, /me/messages/{id}/$value (raw MIME). OAuth 2.0 via Microsoft Entra ID. Scopes: Mail.Read. Works for personal Outlook.com accounts and Microsoft 365 work/school accounts. Incremental sync via deltaLink.
- **Recommendation:** Build now — Microsoft accounts are near-universal; Graph API covers Outlook.com personal, Office 365 work, and Hotmail. Same JSONL correspondence sink as Gmail.
- **Notes:** EWS deadline October 1 2026 — Graph-only going forward. PKCE recommended for public clients. OLM file format (Outlook for Mac local store at ~/Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles/Main Profile) is an alternative M3 path but format is proprietary and poorly documented — Graph is far cleaner. PST files are Windows-only format; Outlook for Mac does not produce PSTs natively.

#### Outlook for Mac (local OLM store) — _Email_

🟠 **Low — OLM/.olk15 is a proprietary Microsoft format with no public spec; parsing requires reverse engineering. The Graph API M5 path covers the same data cleanly for any account that is still online. The OLM path only adds value for fully offline/archived accounts.** · M3 · Full Disk Access (FDA) · effort **L** · 🆕 new

- **Access:** ~/Library/Group Containers/UBF8T346G9.Office/Outlook/Outlook 15 Profiles/Main Profile/ — each message stored as individual .olk15Message file (structured binary, not standard mbox/EML). Export via File → Export → .olm archive (zip of proprietary XML-ish format).
- **Recommendation:** Icebox — use Graph API M5 for live accounts; only revisit if there is strong demand for parsing offline OLM archives.
- **Notes:** OLM is a ZIP of XML-ish files per item — some community parsers exist in Python but no reliable Rust crate. The user can also export to .olm and use a converter to .mbox, then feed the existing mbox importer.

#### ProtonMail — _Email_

🟢 **High — official export tool is open-source, produces standard EML files that existing email.rs already handles (or trivially extends). Bridge path would violate standalone rule (requires Bridge running) so export tool is the right v1.** · M1; M5 fallback via Bridge + IMAP · Proton account credentials; Bridge requires paid Proton plan · effort **M** · 🆕 new

- **Access:** Official export tool (github.com/ProtonMail/proton-mail-export, macOS CLI/GUI binary): exports to EML files + metadata JSON. Auth: Proton login + 2FA. Alternatively, Proton Mail Bridge (paid plan required) creates a local IMAP/SMTP server at 127.0.0.1:1143 that any IMAP client can read.
- **Recommendation:** Build now — ProtonMail is widely used by the privacy-conscious user base Trove targets. Export tool output (EML) feeds directly into existing mbox/EML importer.
- **Notes:** Bridge requires a paid Proton plan and must be running as a separate process — violates standalone rule. The export tool is the clean path: open-source, user-initiated, produces EML. Trove just needs to accept a folder of .eml files in addition to .mbox. Hydroxide (github.com/emersion/hydroxide) is an open-source Go ProtonMail bridge; a Rust port or FFI wrap would make a fully compiled-in M5 path possible but is significant effort — Spike first.

#### Google Chat (Takeout export) — _Team Chat_

🟢 **High — official Takeout mechanism, stable JSON format.** · M1 · none · effort **M** · 🆕 new

- **Access:** Google Takeout → Chat → Download. Exports a Takeout/Google Chat/Groups/<Space>/ folder per space, each with group_info.json + Messages/*.json. Direct messages appear under Takeout/Google Chat/DMs/<conversation>/. Format: JSON arrays of messages with text, sender, timestamp, attachments.
- **Recommendation:** Build now — Google Chat is ubiquitous in Google Workspace orgs; same import pattern as Slack/Discord exports.
- **Notes:** Google Chat API (developers.google.com/workspace/chat) does expose a REST API for reading spaces and messages, but scopes are designed for app bots in Workspace tenants, not personal user history reads. For personal Gmail/Workspace accounts, Takeout is the right path. Hangouts (legacy) also exported via Takeout as Hangouts.json — worth parsing in the same collector since many users have years of Hangouts history.

#### Signal Desktop — _Messaging_

🟡 **Medium — DB is on disk and readable in principle, but the key migration to Keychain (completed mid-2024) means Trove must request the key from the macOS Keychain under Signal's service name, which requires the user to approve keychain access per-app. The sqlcipher Rust crate can then decrypt the DB. Schema is documented by community (Signal-Desktop GitHub). No official API or export exists.** · M3 · Full Disk Access (FDA) + Keychain access for the encryption key · effort **L** · 🆕 new

- **Access:** SQLCipher-encrypted SQLite at ~/Library/Application Support/Signal/sql/db.sqlite. Encryption key: as of mid-2024, stored in macOS Keychain via Electron safeStorage (PR #6849 merged). Previously was plaintext in ~/Library/Application Support/Signal/config.json — users who haven't updated may still have the old key.
- **Recommendation:** Build later — technically feasible but Keychain key access requires careful UX (explaining why Trove needs Signal's key), and Signal's schema changes with app updates. High privacy value but medium complexity.
- **Notes:** Signal has no export feature and no API by design (privacy-first). The only access path is the local SQLCipher DB. Key retrieval: on macOS, Signal stores the encrypted key under the service 'Signal Safe Storage' in Keychain; the Electron safeStorage mechanism uses a per-app Keychain item. Trove would need to ask the user to copy the key or grant keychain access — document this clearly. Signal's schema: tables include conversations, messages, jobs. Community schema references at vmois.dev. Fallback: user can export individual conversations manually via Signal Desktop's 'Export chat' (only to plaintext .txt, not structured). Very lossy — not worth implementing as primary path.

#### WhatsApp (Mac desktop) — _Messaging_

🟡 **Medium — No complete local DB on the Mac app. The practical path for WhatsApp history is: (1) In-app Export: WhatsApp iOS/Mac → individual chat → More → Export Chat → .txt or .zip with media. This gives a text file per chat, parseable but lossy. (2) iPhone backup: the ChatStorage.sqlite in an unencrypted iPhone backup (~/Library/Application Support/MobileSync/Backup/<UUID>) is fully readable — but this overlaps with other Apple backup integrations.** · M1 · none (export); FDA (local cache, limited value) · effort **M** · 🆕 new

- **Access:** WhatsApp for Mac stores some data at ~/Library/Application Support/WhatsApp/ and ~/Library/Caches/WhatsApp/. However, the Mac desktop app is primarily a web wrapper and does NOT store a full message database locally — it caches data temporarily. The full database lives on the iPhone (ChatStorage.sqlite in the iOS app container). On older Macs, ~/Library/Mobile Documents/68Y0128N3~net~whatsapp~WhatsApp/Accounts/<phone>/ contained ChatSearch.sqlite and ChatStorage.sqlite — this path no longer reliably exists in current app versions.
- **Recommendation:** Build later — WhatsApp export is lossy (.txt only, no JSON); iPhone backup path is feasible but overlaps with the iOS backup domain. The signal/noise ratio for Trove is lower than other messaging sources. Track Meta API developments.
- **Notes:** WhatsApp does not provide an official data export API. The Business API (cloud.whatsapp.com) is for businesses only, not personal accounts. In-app export produces a .txt file that is parseable but lacks structure (no sender IDs, no message IDs, just 'Date, Author: Text' lines). iPhone backup (MobileSync) path: ChatStorage.sqlite in a plaintext backup is the richest source — keyed to phone number JIDID, messages table has ZTEXT, ZMESSAGEDATE, etc. This approach requires building an iOS backup reader, which is a separate domain.

#### Facebook Messenger — _Messaging_

🟢 **High — official export, well-structured JSON, covers all DMs and group chats. No API equivalent for personal accounts (Graph API for Messenger is restricted to businesses/platforms).** · M1 · none (user-initiated export) · effort **M** · 🆕 new

- **Access:** Meta Accounts Center: Settings & Privacy → Your Facebook Information → Download Your Information → Messages. Format: JSON (structured, includes sender, timestamp, content, reactions, share URLs) or HTML. Also accessible via messenger.com/your-data.
- **Recommendation:** Build now — Facebook Messenger is one of the most used messaging platforms globally; JSON export is clean and well-structured.
- **Notes:** Export structure: messages/inbox/<ConversationName>_<hash>/message_<N>.json. Each JSON file has 'participants' array and 'messages' array with sender_name, timestamp_ms, content, reactions, photos, share. Files are split into multiple message_1.json, message_2.json etc. per conversation. Character encoding: Meta exports use mojibake-encoded UTF-8 in some fields (Latin-1 decoded as UTF-8) — parser must handle this. No live API for personal accounts; Graph Messenger Platform requires app review + business justification.

#### Instagram DMs — _Social / Messaging_

🟢 **High — same JSON structure as Facebook Messenger export; can likely share the same parser.** · M1 · none · effort **S** · 🆕 new

- **Access:** Instagram Settings → Accounts Center → Your Information and Permissions → Download Your Information. Includes direct_messages/ folder with JSON per thread. Format mirrors Facebook Messenger JSON.
- **Recommendation:** Build now alongside Facebook Messenger — same parser, different folder path in the ZIP.
- **Notes:** Instagram and Facebook share the Accounts Center and use the same export format and JSON schema. Same mojibake encoding caveat applies. The Meta export ZIP contains both if the user has linked accounts. Export can take up to 14 days for large accounts but is usually faster.

#### Microsoft Teams (work/school accounts) — _Team Chat_

🟡 **Medium — Graph API Teams access works well for Microsoft 365 work/school accounts. Personal consumer Microsoft accounts have significantly restricted Teams API access (Group.* permissions not supported for personal accounts). Teams Export API billing was eliminated August 2025.** · M5; M1 fallback · OAuth (Microsoft Entra app registration); work/school account required for full Graph Teams access · effort **M** · 🆕 new

- **Access:** Microsoft Graph API: GET /me/chats (list DMs + group chats), GET /me/chats/{id}/messages (messages), GET /teams/{id}/channels/{id}/messages (channel messages). OAuth 2.0 scopes: Chat.Read, ChannelMessage.Read.All. Also: Microsoft account portal → Privacy → Download your data → includes Teams chat history as JSON.
- **Recommendation:** Build later — primarily valuable for enterprise/work users; personal Microsoft account support is limited. Prioritize after Gmail/Outlook email.
- **Notes:** Personal Microsoft accounts (outlook.com/hotmail) cannot access Teams Chat APIs via Graph in the same way work accounts can. The Microsoft account privacy download (account.microsoft.com → Privacy) does include some Teams history as JSON — this M1 path works for personal accounts. For work accounts, Graph API provides full programmatic access. EWS deprecated October 2026 means Graph is the only API path.

#### X (Twitter) DMs — _Social / Messaging_

🟢 **High — official archive mechanism includes DMs. JSON structure is parseable once the JS wrapper is stripped. Note the X API v2 DM endpoints exist (Chat.Read scope) but require paid developer account tier for meaningful access.** · M1 · none · effort **M** · 🆕 new

- **Access:** Official data archive: X Settings → Your Account → Download an archive of your data. ZIP includes data/direct-messages.js and data/direct-messages-group.js with full DM history in JSON-ish format (JS files with variable assignment prefix to strip). Contains: conversation ID, participants, messages with text and timestamps.
- **Recommendation:** Build now — Twitter/X archive import is valuable and shares the same import pipeline pattern as other social exports.
- **Notes:** Archive format: files are JavaScript modules (e.g. window.YTD.direct_messages.part0 = [...]) — strip the assignment prefix, parse JSON. Each message object has conversationId, senderId, text, createdAt, mediaUrls. The API path (OAuth 2.0, dm.read scope) requires X developer account — Tier 1 (free) has severe rate limits; Tier 2 ($100/month) is needed for meaningful pulls. The archive export is the right path for personal use.

#### GroupMe — _Messaging_

🟢 **High — simple REST API, personal token, no complex OAuth. GroupMe is popular in US universities and sports teams.** · M5 · API token (from dev.groupme.com — no app registration needed) · effort **M** · 🆕 new

- **Access:** GroupMe API v3: https://api.groupme.com/v3/groups (list groups), /v3/groups/{id}/messages (fetch messages, paginate with before_id). Auth: access token from dev.groupme.com (login → Access Token). No OAuth dance — just a token in request headers.
- **Recommendation:** Build later — smaller user base than other platforms; keyless personal token makes it easy when prioritized.
- **Notes:** API caching restriction in ToS: only cache up to 100 messages or 24 hours worth for 3 days, then must re-request. For Trove's archival use case (writing to vault and not re-querying the API), this ToS clause is awkward but the practical enforcement is nil for personal archiving. GroupMe also offers a data export (Settings → Export Data) producing a ZIP with JSON files — an M1 alternative that avoids the ToS ambiguity. GroupMe is owned by Microsoft.

#### WeChat (macOS desktop) — _Messaging_

🟠 **Low — key extraction requires a running WeChat process and lldb memory scanning, violating the standalone rule. The Chatlog CLI tool was discontinued in October 2025 due to WeChat policy compliance issues. No official export API or data export feature. WeChat Terms of Service prohibit third-party data extraction.** · M3 · Full Disk Access (FDA) + running WeChat process for key extraction · effort **XL** · 🆕 new

- **Access:** SQLCipher-encrypted SQLite databases at ~/Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files/<account>/db_storage/. Encryption key is a 32-byte value derived from the user's WeChat ID + machine UUID, cached in process memory during runtime. Key extraction requires attaching lldb to the running WeChat process to scan memory (wechat-db-decrypt-macos approach). As of WeChat 4.x (2025), 24 separate per-database keys.
- **Recommendation:** Icebox — key extraction requires the app running (violates standalone rule), ToS prohibits it, and the primary tooling was discontinued. Revisit only if WeChat adds an official export.
- **Notes:** Third-party tools like WechatExplorer and wx-cli exist and work but require the WeChat process to be running for key extraction. The key is not stored on disk — it is derived at runtime. This makes WeChat fundamentally different from Signal (which does store the key on disk, now in Keychain). WeChat has ~800M users but is primarily East Asian — lower priority for a general Western user base launch.

#### LINE (macOS desktop) — _Messaging_

🟠 **Low — no public documentation of the local database format or path. LINE is sandboxed so the DB path is only accessible with FDA. No official export/API for personal messages. LINE's API is business-oriented (LINE Developers, Messaging API for bots only). Local DB format is unknown/undocumented.** · M3 · FDA (sandboxed container path) · effort **L** · 🆕 new

- **Access:** LINE for macOS (Mac App Store). As of version 9.8.0 (April 2025), LINE desktop saves all chat history received on PC locally. Data location: sandboxed app container, likely ~/Library/Containers/jp.naver.line.mac/. In-app backup: Chat menu → Back Up Chat History (backs up to LINE's servers, not local files). No official export to local file.
- **Recommendation:** Icebox — no viable access path without reverse engineering the DB format. Revisit if community tooling emerges or if LINE adds export functionality.
- **Notes:** LINE has ~200M users primarily in Japan, Thailand, Taiwan. Lower priority for initial Western launch. LINE's Messaging API is for chatbots, not personal message access.

#### Beeper (unified messaging) — _Messaging Aggregator_

🟡 **Medium — Beeper Desktop API is documented and local-only, which is great. However, it requires Beeper to be installed AND running (standalone rule violation). Beeper is a paid app ($X/month). Bridging varies — some networks (WhatsApp, Signal) use on-device connections; others may relay through Beeper's servers.** · M3 · none (localhost API, no TCC) · effort **M** · 🆕 new

- **Access:** Beeper Desktop API — a fully local HTTP API (localhost) for all connected networks (WhatsApp, Instagram, Telegram, Google Messages, Signal, LinkedIn, X, Discord, Slack, etc.). SDK for JavaScript/Python/Go/PHP. No auth required beyond localhost. Read chats: GET /api/v1/chats, messages: GET /api/v1/chats/{id}/messages.
- **Recommendation:** Spike first — if the user already uses Beeper, this is a remarkably elegant way to access many messaging services at once via a single local API. But it violates the standalone rule (Beeper must be running). Practical for an M6 agent-style collector that the user opts into knowing they need Beeper running.
- **Notes:** Beeper's 'On-Device Connections' mode means messages for WhatsApp and Signal flow directly device-to-network — not through Beeper servers. This is strong for privacy. The Beeper Desktop API localhost approach is similar to how Trove reads other app databases, except it's an HTTP API rather than direct SQLite access. Could be framed as an opt-in 'enhanced mode' requiring Beeper. Texts.app ($149/year, macOS-first, iMessage native) is a competitor but does not expose a local API.

#### Matrix / Element — _Messaging_

🟢 **High — Matrix Client-Server API is a published open spec (spec.matrix.org), the sync endpoint is the standard incremental fetch, and access tokens are long-lived. Works for any homeserver (matrix.org, self-hosted, etc.).** · M5 · Matrix access token (from Element/client login); no macOS TCC needed · effort **M** · 🆕 new

- **Access:** Matrix Client-Server API (spec.matrix.org): GET /_matrix/client/v3/sync (incremental sync with since token), GET /_matrix/client/v3/rooms/{roomId}/messages. Auth: access token from any Matrix homeserver (element.io, matrix.org, self-hosted). Also: element-hq/matrix-archive exports room history to JSON/HTML/YAML.
- **Recommendation:** Build later — Matrix is growing but remains niche outside open-source/tech communities. The API is clean and worth adding; lower priority than mainstream platforms.
- **Notes:** Matrix uses room_id not conversation-level threading. The /sync endpoint returns all events since a token — store the since token as a watermark. End-to-end encrypted rooms require the client's encryption keys (stored in Element's local IndexedDB or a key backup) — handling E2EE messages requires the vodozemac Rust crate (Matrix Rust SDK). Non-E2EE rooms are straightforward. The matrix-sdk Rust crate (github.com/matrix-org/matrix-rust-sdk) provides a high-level client — significant compile-time addition but handles E2EE correctly.

#### Discord (DiscordChatExporter JSON import) — _Gaming / Community Chat_

🟢 **High — DiscordChatExporter is widely used and produces clean JSON. Trove only needs to parse the output format; the user runs the tool themselves.** · M1 · none (Trove side) · effort **S** · 🆕 new

- **Access:** User runs DiscordChatExporter (github.com/Tyrrrz/DiscordChatExporter) with their user token locally — outputs JSON files per channel/DM with full thread context (both sides of conversation, unlike the official data package). Trove imports the resulting JSON folder.
- **Recommendation:** Build now as a complement to the official data package import — covers the critical gap (received messages in full thread context). Trove does NOT run DiscordChatExporter; the user does and drops the output folder.
- **Notes:** DiscordChatExporter uses self-botting (user token, not bot token) which violates Discord ToS — but that is the user's choice with their own account. Trove only reads the output. JSON schema: channel.json (metadata) + messages.json array with id, timestamp, author.name, content, attachments, reactions. Reliable dedup on message id.

#### IRC (ZNC / WeeChat logs) — _Messaging_

🟢 **High — pure plaintext log files, no encryption, no permissions beyond home directory access. Niche but trivial to implement.** · M3 · none (home directory paths) · effort **S** · 🆕 new

- **Access:** ZNC bouncer logs: ~/.znc/users/<user>/networks/<net>/moddata/log/<channel>/YYYY-MM-DD.log (plain text). WeeChat logs: ~/.weechat/logs/<network>.<channel>.weechatlog (plain text, timestamped). Irssi: ~/.irssi/logs/. All are plain text with consistent timestamp format.
- **Recommendation:** Build later — very small user base in 2026, but trivially easy (plaintext log parser). Good signal for open-source/developer users.
- **Notes:** Log format varies by client but is consistent within each client: '[HH:MM:SS] <nick> message'. A simple regex-based parser handles all three major IRC clients. ZNC also supports an IRC v3 self-message log extension. libera.chat and OFTC are the main active IRC networks in 2026.

#### ProtonMail (native export tool — EML/JSON) — _Email_

🟢 **High — official tool, standard EML output, feeds directly into existing mbox/EML import. Works on free plans (no paid requirement unlike Bridge).** · M1 · Proton account credentials (entered by user into the export tool) · effort **S** · 🆕 new

- **Access:** github.com/ProtonMail/proton-mail-export — official open-source CLI/GUI, macOS build available. Login with Proton credentials + 2FA. Exports all mailboxes as EML files + metadata JSON. No Bridge dependency.
- **Recommendation:** Build now — just extend the existing email importer to accept a folder of .eml files in addition to a single .mbox file. ProtonMail is very popular with privacy-focused users.
- **Notes:** The export tool is separate from the Bridge. It produces: one .eml per message + a metadata.json per message with labels/folder info. Free accounts can use it. Trove only needs to add a 'folder of EML files' import path alongside the existing mbox path — trivial extension of email.rs.

### Email & Messaging Apps — cross-cutting notes

1. UNIFIED CORRESPONDENCE SINK: Trove already has a well-designed correspondence/YYYY-MM.jsonl format (used by iMessage, email mbox, Slack export). All new email and messaging sources should write to this same sink with a service field discriminating the source. The existing email.rs dedup-by-Message-ID pattern generalizes perfectly — each source needs its own guid scheme (Telegram: message.id, Discord: message.id, Facebook: timestamp_ms+sender, etc.).

2. ENCRYPTION KEY BARRIER: Signal, WeChat, and (legacy) Telegram local DBs are all SQLCipher-encrypted. Each has a different key storage mechanism. Signal now uses macOS Keychain (safeStorage), making it the most principled but requiring Keychain access UI. WeChat requires a running process (blocks standalone rule). Telegram macOS native uses a tempkeyEncrypted file. Where the key is not on disk without the app running, the local DB path is effectively blocked for Trove.

3. OAUTH CONSOLIDATION: Gmail, Outlook/Graph, Slack, and Matrix all use OAuth 2.0 / Bearer tokens. The existing generic OAuth flow in crates/trove-core/src/sync/oauth.rs should serve as the base for all new M5 email/messaging collectors. The bring-your-own client_id fallback pattern (already used by Oura/TickTick) should be documented and applied consistently. Compiled-in app credentials per docs/oauth-distribution.md remain the UX ideal.

4. IMPORT PATTERNS REUSE: Facebook Messenger, Instagram DMs, Discord data package, X archive, and Google Chat Takeout all follow the same pattern: user requests a ZIP from a settings page, receives it within hours/days, drops it on Trove. A single 'import ZIP' UI flow with format auto-detection would serve all of these. The mojibake encoding bug in Meta exports (Latin-1 decoded as UTF-8 in some string fields) affects both Messenger and Instagram — handle once, fix both.

5. INCREMENTAL SYNC WATERMARKS: For M5 sources, each collector needs a per-account watermark stored in .trove/sync/<service>-watermarks.json. Gmail uses historyId, Outlook uses deltaLink, Matrix uses sync token, Slack uses message ts, IMAP uses UID VALIDITY + max UID. The watermark pattern is already established by Oura (oura-sync.json) and TickTick.

6. FULL DISK ACCESS GATE: Apple Mail (Envelope Index + emlx), Signal (db.sqlite), and any future macOS local DB reads all require FDA. FDA is already on the permission ladder for iMessage. The existing FDA gate/prompt pattern from imessage.rs should be reused without requesting it multiple times.

7. WHAT IS UNRECOVERABLE IF NOT CAPTURED LIVE: For all platform exports (Telegram, Facebook, Discord data package), the export captures all history including deleted messages that the platform still retains — but once a message is deleted server-side and the user has not yet exported, it is gone forever. For IMAP-based sources, messages deleted from server (and not in local archive) are gone. Encourage users to do initial full exports early.

8. SLACK DM GAP: The existing Slack workspace export import only captures public channels (free/Pro plan). The new Slack API pull M5 path (conversations.history with user token) fills this gap for DMs and private channels the user is a member of, without requiring admin/Business+ — high priority complement to the existing import.

---

## Calls, Voice & Meeting Transcripts

This domain spans three overlapping layers: (1) phone/video call metadata and audio already flowing through macOS local DBs (CallHistoryDB — built; voicemail — new); (2) AI meeting-recorder services (Granola/Fathom — planned; Zoom, Fireflies, Otter, Read.ai, Krisp, tl;dv, Webex, Teams — all new); and (3) Voice Memos, which is partially planned but the transcript extraction path is now well-characterized. The overall feasibility for Trove is high: every major meeting-recorder service now offers an API or MCP server, Granola and Fathom already have MCP integrations noted in the HANDOFF, and the macOS local-DB sources (Voice Memos, voicemail in iPhone backups) are clearly accessible under FDA. The main constraints are plan-gating (Granola transcripts need Business plan; Otter API needs Enterprise; Teams no longer metered but requires a work/school AAD account) and the fundamental fact that call audio isn't transcribed anywhere locally by default — Zoom AI Companion and Biome-class sources are cloud-processed before arriving on the device.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Calls & FaceTime (CallHistoryDB) | Call Metadata | M3 | Full Disk Access (same binary grant as Messages) | S | 🟢 High — built and validated. Syncs iPhone + FaceTime + Mac cellular relay calls via Continuity. Multi-year retention observed. | ✅ built |
| Fathom Video Notetaker | Meeting Transcripts | M5 | API key (user-level; key only accesses meetings recorded by that user or shared to their team). No specific paid-plan requirement mentioned in public docs. | S | 🟢 High — public API with TypeScript/Python SDKs, webhooks, and multiple MCP implementations. Rate limit: 60 calls/min. Transcripts include speaker labels and timestamps. Trove already has mcp__fathom__ MCP tools in the deferred list. | 📋 planned |
| Zoom Cloud Recordings & Transcripts | Meeting Transcripts | M5 | OAuth (user-level app; recording:read scope). Requires Pro or higher Zoom plan to have cloud recording. Download tokens expire in 24h but the recording list endpoint re-issues them. | M | 🟢 High — mature, well-documented API. VTT transcripts downloadable with bearer token. Main complexity: transcript only exists when host enabled the setting pre-meeting, and AI Companion summaries are only accessible if the user is the host (non-host attendee access is a known developer forum pain point as of 2026). | 🆕 new |
| Fireflies.ai | Meeting Transcripts | M5 | API key (available on all plans including free). No paid-plan gating on transcript access documented. | S | 🟢 High — GraphQL API is fully live, well-documented, returns rich sentence-level transcripts with speaker attribution, analytics, and structured summaries. Webhooks for real-time push available. | 🆕 new |
| Apple Voice Memos | Voice | M3 | Full Disk Access (Group Containers path is TCC-gated) | S | 🟢 High — path is confirmed, schema is well-characterized, native transcript extraction is pure file parsing (no external ML needed on macOS 15+). Audio files can optionally be run through a bundled Whisper model for pre-Sequoia recordings. | 📋 planned |
| Granola Meeting Notes | Meeting Transcripts | M5 | OAuth / API key (user-level; available on Business/Enterprise plans). MCP approach requires user to run Granola app and have an MCP-capable client. | S | 🟢 High — REST API live, well-documented (docs.granola.ai), stable rate limits (25 req/5s burst, 300/min sustained). Only returns notes that have an AI summary; raw transcript access requires Business plan ($14/mo). MCP is available on all plans but transcripts gate on Business+. | 📋 planned |
| Visual Voicemail (iPhone backup) | Calls | M3 | Full Disk Access for MobileSync/Backup path | M | 🟡 Medium — data exists on-disk in iTunes/Finder backups but requires an unencrypted local backup (or the backup encryption password). Many users use iCloud backups instead of local ones, making this path unavailable. The AMR audio format needs transcoding. Apple's transcript PLIST provides machine-confidence-rated text but is sparse. | 🆕 new |
| Read.ai | Meeting Transcripts | M5 | API key. MCP and REST API are open beta (no specific paid plan requirement stated for basic access). Webhooks require Enterprise+. | S | 🟢 High — public REST API and official MCP server both live. Returns transcripts, summaries, speaker analytics. Open beta status means API may have breaking changes. | 🆕 new |
| Google Meet Recordings & Transcripts | Meeting Transcripts | M5 | OAuth (Google account; meetings.space.readonly scope). Requires Google Workspace or personal Google account. Transcription is off by default; organizer must enable it. Transcript entries deleted 30 days after conference ends (Drive Doc persists). | M | 🟡 Medium — API is well-documented and live, but transcription must be manually enabled per meeting, and transcript entries have a 30-day API retention limit (Drive Doc is the durable copy). Non-organizers cannot access recordings via the API (Drive file permissions apply). | 🆕 new |
| Google Voice (Takeout export) | Calls | M1 | Google account login (no macOS permission needed). User-initiated export. | S | 🟢 High for what's available — Takeout export is reliable and includes voicemail transcripts + audio. The gap is that call recordings and real-time call transcripts do not exist in Google Voice (it only transcribes voicemails). | 🆕 new |
| Zoom Local Recordings | Meeting Transcripts | M3 | None — user's own Documents folder. No TCC needed. | S | 🟢 High — plain files in Documents, no permissions needed, VTT is a standard parseable format. Limitation: VTT only exists when the host saved closed captions; this is off by default. | 🆕 new |
| Microsoft Teams Transcripts | Meeting Transcripts | M5 | OAuth (Azure AD / Microsoft account). Work or school account required for most features (personal Microsoft accounts have limited API access). OnlineMeetingTranscript.Read.All scope. | M | 🟡 Medium — API is GA and now free (no longer metered). Main constraint: requires an Azure AD (work/school) account; personal Microsoft accounts have very limited Graph API coverage for meetings. Transcript only available after meeting ends, and only for meetings where transcription was enabled. | 🆕 new |
| Krisp Meeting Notes | Meeting Transcripts | M5 | Krisp account (no specific plan tier documented for webhook; noise cancellation features are free-tier limited). Webhook endpoint must be reachable from Krisp's servers. | M | 🟡 Medium — webhook delivery requires Trove to run an HTTPS endpoint reachable from the internet, which violates the standalone/local-first constraint unless mediated by a local relay or the user manually exports. The MCP server is a better local-first path if Krisp runs the server locally. | 🆕 new |
| tl;dv Meeting Recorder | Meeting Transcripts | M5 | API key (Business plan required). OAuth not mentioned; API key auth. | S | 🟡 Medium — API exists and is documented, but requires the Business plan. Same webhook-only issue as Krisp (requires a reachable endpoint for real-time; polling fallback not confirmed). | 🆕 new |
| Otter.ai | Meeting Transcripts | M5 | Enterprise plan + account manager approval. For individuals: M1 manual export (TXT/DOCX). | L | 🟠 Low for individuals — API is Enterprise-only and requires contacting Otter's sales team. M1 export (TXT/DOCX/SRT) is available to all paid plans but is manual-only. No OAuth self-service path. | 🆕 new |
| Webex (Cisco) Meetings | Meeting Transcripts | M5 | OAuth (Webex account). meeting:read scope. Free and paid Webex accounts supported. | M | 🟢 High for Webex users — well-documented REST API, transcript download links in VTT/TXT format, and the API now supports both Webex Assistant and Cisco AI Assistant transcripts. Webex is primarily enterprise/corporate use. | 🆕 new |
| Skype Call History | Calls | M1 | Microsoft/Skype account login. Export deadline was June 2026. | S | 🟠 Low — Skype is shut down (May 2025). The export window may have closed. The remaining value is one-time historical import for users who had Skype call history. | 🆕 new |
| WhatsApp Calls (Mac Desktop app) | Calls | M1 | M1 (manual export from iPhone). Full Disk Access for macOS app cache (incomplete data only). | M | 🟠 Low — no complete local database of WhatsApp calls accessible on macOS. The iPhone app's call history is not exported via per-chat export (only messages). WhatsApp has no public API for personal data. End-to-end encryption prevents server-side access. The data-sources.md already categorizes this as M1 icebox for messages; calls are even harder. | 🆕 new |

### Detail

#### Calls & FaceTime (CallHistoryDB) — _Call Metadata_

🟢 **High — built and validated. Syncs iPhone + FaceTime + Mac cellular relay calls via Continuity. Multi-year retention observed.** · M3 · Full Disk Access (same binary grant as Messages) · effort **S** · ✅ built

- **Access:** ~/Library/Application Support/CallHistoryDB/CallHistory.storedata — Core Data SQLite, ZCALLRECORD table; columns include ZDATE (Apple epoch float), ZDURATION, ZADDRESS, ZSERVICE_PROVIDER, ZORIGINATED, ZANSWERED, ZSPAM. Already reversed in calls.rs.
- **Recommendation:** Build now
- **Notes:** Already fully implemented in crates/trove-core/src/calls.rs. Writes correspondence/calls/YYYY-MM.jsonl. The remaining gap is voicemail (separate row below). macOS Tahoe adds a native Phone app that syncs call history to the same DB via Continuity.

#### Granola Meeting Notes — _Meeting Transcripts_

🟢 **High — REST API live, well-documented (docs.granola.ai), stable rate limits (25 req/5s burst, 300/min sustained). Only returns notes that have an AI summary; raw transcript access requires Business plan ($14/mo). MCP is available on all plans but transcripts gate on Business+.** · M5 · OAuth / API key (user-level; available on Business/Enterprise plans). MCP approach requires user to run Granola app and have an MCP-capable client. · effort **S** · 📋 planned

- **Access:** REST API: GET /v1/notes and GET /v1/notes/{id}. Base URL from docs.granola.ai. Bearer token auth (API key from Granola settings). MCP server: official Granola MCP (listed at granola.ai/blog/granola-mcp); also community implementations on GitHub (mishkinf/granola-mcp, chrisguillory/granola-mcp). Trove already has MCP tools loaded (Granola MCP in the deferred tools list).
- **Recommendation:** Build now — M5 REST pull is the cleanest path. Write meetings/granola/YYYY-MM.jsonl with title, attendees, summary, transcript (where available). M6 MCP fallback for users on Basic plan (summaries only).
- **Notes:** Personal API key is user-level: only returns notes owned by or shared with the requesting user. Enterprise API (admin-level) is separate. Granola raised $125M in March 2026 at $1.5B valuation — unlikely to shut down. The Trove MCP tools list already includes mcp__claude_ai_Granola__ tools, making an M6 agent collector trivially buildable today as v1.

#### Fathom Video Notetaker — _Meeting Transcripts_

🟢 **High — public API with TypeScript/Python SDKs, webhooks, and multiple MCP implementations. Rate limit: 60 calls/min. Transcripts include speaker labels and timestamps. Trove already has mcp__fathom__ MCP tools in the deferred list.** · M5 · API key (user-level; key only accesses meetings recorded by that user or shared to their team). No specific paid-plan requirement mentioned in public docs. · effort **S** · 📋 planned

- **Access:** REST API at https://api.fathom.ai/external/v1. Auth: X-Api-Key header (API key from Fathom settings). Key endpoints: GET /meetings (list with include_highlights=true), GET /meetings/{id}/transcript. Webhooks fire on meeting completion. Multiple MCP implementations on GitHub (Dot-Fun/fathom-mcp, druellan/Fathom-Simple-MCP, matthewbergvinson/fathom-mcp). Official MCP docs at developers.fathom.ai/mcp-docs.
- **Recommendation:** Build now — same M5 REST pattern as Granola. Use webhooks as the live trigger and REST poll for backfill. Write meetings/fathom/YYYY-MM.jsonl.
- **Notes:** API key access is per-user; admin keys do not reach other users' unshared meetings. Async processing means transcripts are not available immediately after a call ends. The Trove deferred tools list includes mcp__fathom__authenticate and mcp__fathom__complete_authentication, suggesting MCP integration is already partially wired.

#### Zoom Cloud Recordings & Transcripts — _Meeting Transcripts_

🟢 **High — mature, well-documented API. VTT transcripts downloadable with bearer token. Main complexity: transcript only exists when host enabled the setting pre-meeting, and AI Companion summaries are only accessible if the user is the host (non-host attendee access is a known developer forum pain point as of 2026).** · M5 · OAuth (user-level app; recording:read scope). Requires Pro or higher Zoom plan to have cloud recording. Download tokens expire in 24h but the recording list endpoint re-issues them. · effort **M** · 🆕 new

- **Access:** REST API v2 at https://api.zoom.us/v2/. Key endpoint: GET /users/me/recordings (list with date range). Each recording_files array entry includes a file with file_type='TRANSCRIPT' (VTT) and a download_url + download_access_token. OAuth 2.0 user-level app; scope: recording:read. Cloud transcript requires the host to have enabled 'Audio Transcript' in Zoom settings (disabled by default). Zoom AI Companion summaries accessible via separate endpoint when AI Companion is enabled on the account.
- **Recommendation:** Build now — extremely high value. Zoom is the dominant work meeting platform. Pull meeting list + download VTT transcripts + AI Companion summaries (if available). Store as meetings/zoom/YYYY-MM.jsonl with VTT saved alongside.
- **Notes:** Local recordings also exist: ~/Documents/Zoom/ on macOS, VTT transcript co-located (M3 fallback, no OAuth needed). The local path is a useful complement — catches meetings recorded locally when cloud recording was off. Zoom AI Companion summaries are only available to the meeting host via the API; non-host attendees cannot fetch them through user-level OAuth (open developer forum issue as of 2026).

#### Fireflies.ai — _Meeting Transcripts_

🟢 **High — GraphQL API is fully live, well-documented, returns rich sentence-level transcripts with speaker attribution, analytics, and structured summaries. Webhooks for real-time push available.** · M5 · API key (available on all plans including free). No paid-plan gating on transcript access documented. · effort **S** · 🆕 new

- **Access:** GraphQL API at https://api.fireflies.ai/graphql. Auth: Authorization: Bearer <api_key> header. Query transcripts (list, filter by date), get transcript by ID (returns sentences with speaker/start_time/end_time/text, summaries with action_items/keywords/outline, analytics). Official MCP server documented at docs.fireflies.ai/getting-started/mcp-configuration. Open-source MCP: Props-Labs/fireflies-mcp on GitHub.
- **Recommendation:** Build now — high value, low friction. GraphQL makes it easy to fetch exactly the fields needed. Store as meetings/fireflies/YYYY-MM.jsonl.
- **Notes:** Fireflies joins all meetings as a bot participant (records audio, then transcribes). The API returns data only for meetings the user's Fireflies account attended. Sentence-level data includes AI-generated action item, question, and sentiment tags per sentence — richer than most competitors.

#### Apple Voice Memos — _Voice_

🟢 **High — path is confirmed, schema is well-characterized, native transcript extraction is pure file parsing (no external ML needed on macOS 15+). Audio files can optionally be run through a bundled Whisper model for pre-Sequoia recordings.** · M3 · Full Disk Access (Group Containers path is TCC-gated) · effort **S** · 📋 planned

- **Access:** Local DB: ~/Library/Group Containers/group.com.apple.VoiceMemos.shared/Recordings/CloudRecordings.db (SQLite). Audio files in same directory as YYYYMMDD HHMMSS[-hash].m4a or .qta. Native transcripts (macOS 15 Sequoia+) are embedded in the .m4a as a tsrp UDTA atom containing JSON ({"attributedString":...}) — extractable without Whisper by scanning the binary atom. Reference implementation: github.com/pedramamini/voice-memos-gist, github.com/jwulff/apple-voice-memo-mcp.
- **Recommendation:** Build now — already marked planned in data-sources.md; the tsrp atom path is the key new finding. macOS 15+ native transcripts make this zero-dependency. Write voice-memos/YYYY-MM.jsonl with title, duration, transcript text, file path.
- **Notes:** macOS 15 (Sequoia) required for native on-device transcripts. Pre-15 recordings need a bundled Whisper model (whisper.rs/candle crate) — consider this an opt-in enrichment pass. The MCP server at github.com/jwulff/apple-voice-memo-mcp provides a ready reference for the atom parsing logic. FDA grant already held by troved. iCloud sync means iPhone Voice Memos appear in the same directory.

#### Visual Voicemail (iPhone backup) — _Calls_

🟡 **Medium — data exists on-disk in iTunes/Finder backups but requires an unencrypted local backup (or the backup encryption password). Many users use iCloud backups instead of local ones, making this path unavailable. The AMR audio format needs transcoding. Apple's transcript PLIST provides machine-confidence-rated text but is sparse.** · M3 · Full Disk Access for MobileSync/Backup path · effort **M** · 🆕 new

- **Access:** iPhone backup path: ~/Library/Application Support/MobileSync/Backup/<device-UUID>/. File HomeDomain/Library/Voicemail/voicemail.db (SQLite, contains sender, date, duration, transcript confidence). Audio files are .amr format (one per voicemail, numbered by primary key). Transcript files are binary PLIST (.transcript extension), keyed by the same row ID. Alternatively, iCloud Drive backup or extraction tools (iMazing, iPhone Backup Extractor) can surface these.
- **Recommendation:** Build later — depends on user having local iPhone backups. Valuable when available. Implement as an optional enrichment: detect backup → extract voicemail.db → import metadata + transcripts.
- **Notes:** iCloud backups are encrypted and stored remotely; cannot be read on disk without Apple's private key. Only unencrypted local backups (made via Finder/iTunes) expose this path. The .transcript PLIST format provides word-by-word confidence scores. AMR audio can be decoded via ffmpeg (bundled as a Rust binding via ffmpeg-next crate) for a Whisper pass if desired. This is distinct from the iPhone 'Live Voicemail' real-time transcription (iOS 17+), which isn't persisted to disk in a readable form.

#### Read.ai — _Meeting Transcripts_

🟢 **High — public REST API and official MCP server both live. Returns transcripts, summaries, speaker analytics. Open beta status means API may have breaking changes.** · M5 · API key. MCP and REST API are open beta (no specific paid plan requirement stated for basic access). Webhooks require Enterprise+. · effort **S** · 🆕 new

- **Access:** REST API (open beta): base URL at support.read.ai/hc/en-us/articles/49381161088659. Auth: Bearer token (API key). Endpoints: list meetings, get meeting report (transcript, summary, action items, speaker stats). MCP server: official Read AI MCP (read.ai/post/read-ai-mcp). Webhooks available on Enterprise+ plan.
- **Recommendation:** Build later — lower priority than Granola/Fathom/Fireflies/Zoom but straightforward to add once the meetings/YYYY-MM.jsonl pattern is established. Use same M5 connector pattern.
- **Notes:** Read.ai is popular in enterprise Zoom/Teams/Google Meet workflows. Historical export: the API is the most flexible path for bulk history; Zapier integration only captures new meetings going forward. Open beta API — pin to a version and monitor for changes.

#### Google Meet Recordings & Transcripts — _Meeting Transcripts_

🟡 **Medium — API is well-documented and live, but transcription must be manually enabled per meeting, and transcript entries have a 30-day API retention limit (Drive Doc is the durable copy). Non-organizers cannot access recordings via the API (Drive file permissions apply).** · M5 · OAuth (Google account; meetings.space.readonly scope). Requires Google Workspace or personal Google account. Transcription is off by default; organizer must enable it. Transcript entries deleted 30 days after conference ends (Drive Doc persists). · effort **M** · 🆕 new

- **Access:** Google Meet REST API v2: GET /v2/conferenceRecords (list conferences), GET /v2/conferenceRecords/{id}/transcripts (list transcripts), GET /v2/conferenceRecords/{id}/transcripts/{id}/entries (fetch utterance-level entries). Transcript entries are also saved as a Google Doc in the organizer's Drive; exportUri field in DocsDestination gives the Drive export URL. OAuth scope: https://www.googleapis.com/auth/meetings.space.readonly.
- **Recommendation:** Build later — valuable for Google Workspace users. Pair with the Google Drive integration (already planned) to fetch the transcript Doc as the durable copy. The Google OAuth integration (partially built in the 'google' worktree) provides the auth scaffolding.
- **Notes:** The 30-day API retention on transcript entries means Trove must pull promptly or rely on the Drive Doc copy. Recordings are stored in the organizer's Drive, not the participant's — OAuth does not override Drive sharing permissions. The Google Meet API is part of Google Workspace, so it covers Google Calendar-scheduled meetings including personal @gmail.com accounts.

#### Google Voice (Takeout export) — _Calls_

🟢 **High for what's available — Takeout export is reliable and includes voicemail transcripts + audio. The gap is that call recordings and real-time call transcripts do not exist in Google Voice (it only transcribes voicemails).** · M1 · Google account login (no macOS permission needed). User-initiated export. · effort **S** · 🆕 new

- **Access:** Google Takeout: takeout.google.com → select 'Voice' → download archive. Format: HTML files per conversation (calls, voicemails, texts) plus .mp3 audio for voicemails. Voicemail transcripts are embedded in the HTML. Call log shows date/time/duration/direction but no transcript (Google Voice does not transcribe calls). No official public API for Voice data.
- **Recommendation:** Build now — low effort (HTML parser for the Takeout format). Voicemail transcripts are high value. Call log fills the gap for Google Voice users who don't use Apple's phone. Write correspondence/calls/google-voice/YYYY-MM.jsonl.
- **Notes:** The Takeout HTML format is stable and well-characterized (community parsers like voice2json on GitHub). No API exists for programmatic pull — Takeout-only. Voicemail .mp3 files can optionally be re-transcribed with a bundled Whisper model for higher accuracy than Google's built-in transcription.

#### Zoom Local Recordings — _Meeting Transcripts_

🟢 **High — plain files in Documents, no permissions needed, VTT is a standard parseable format. Limitation: VTT only exists when the host saved closed captions; this is off by default.** · M3 · None — user's own Documents folder. No TCC needed. · effort **S** · 🆕 new

- **Access:** macOS local path: ~/Documents/Zoom/<meeting-name>/. Files: .mp4 (video), .m4a (audio only), .txt (chat log), .vtt (transcript, if 'Save closed caption as VTT file' was enabled in Zoom settings). The VTT file is plain text with timestamps and speaker-attributed utterances.
- **Recommendation:** Build now as a companion to the Zoom cloud API collector. This catches meetings recorded locally when cloud recording was off or the user doesn't have a Pro plan. Watch ~/Documents/Zoom/ for new directories.
- **Notes:** Directory structure is <meeting-name> + date stamp. VTT format is standard WebVTT — a simple parser suffices. The M3 and M5 Zoom collectors write to the same meetings/zoom/ path; dedup by meeting UUID (present in the VTT metadata line or the directory name).

#### Microsoft Teams Transcripts — _Meeting Transcripts_

🟡 **Medium — API is GA and now free (no longer metered). Main constraint: requires an Azure AD (work/school) account; personal Microsoft accounts have very limited Graph API coverage for meetings. Transcript only available after meeting ends, and only for meetings where transcription was enabled.** · M5 · OAuth (Azure AD / Microsoft account). Work or school account required for most features (personal Microsoft accounts have limited API access). OnlineMeetingTranscript.Read.All scope. · effort **M** · 🆕 new

- **Access:** Microsoft Graph API: GET /me/onlineMeetings/{meetingId}/transcripts (list), GET /me/onlineMeetings/{meetingId}/transcripts/{transcriptId}/content (download VTT or text). Endpoint also available via /users/{userId}/onlineMeetings. Auth: OAuth (Azure AD); scopes: OnlineMeetingTranscript.Read.All or OnlineMeetingTranscript.Read. As of August 25, 2025, transcript APIs are no longer metered (previously required Azure billing subscription).
- **Recommendation:** Build later — valuable for enterprise/work users with Teams-heavy workflows. The Azure AD OAuth flow is more complex than consumer OAuth. Consider bundling with a broader Microsoft 365 integration (Outlook calendar, OneDrive).
- **Notes:** Transcript content is VTT format. Recording is separate (callRecording resource) and stored in OneDrive/SharePoint. Transcript APIs are for scheduled online meetings only — not ad-hoc calls or channel meetings. Personal Microsoft accounts cannot use these Graph endpoints.

#### Krisp Meeting Notes — _Meeting Transcripts_

🟡 **Medium — webhook delivery requires Trove to run an HTTPS endpoint reachable from the internet, which violates the standalone/local-first constraint unless mediated by a local relay or the user manually exports. The MCP server is a better local-first path if Krisp runs the server locally.** · M5 · Krisp account (no specific plan tier documented for webhook; noise cancellation features are free-tier limited). Webhook endpoint must be reachable from Krisp's servers. · effort **M** · 🆕 new

- **Access:** Webhook API: configure a webhook URL in Krisp settings; Krisp POSTs transcript/notes/outline JSON payloads on meeting completion. Export: .txt transcript download from the Krisp dashboard. MCP server: official Krisp MCP (mentioned in Krisp 3.10.5 release notes). No documented REST polling API; webhook is the primary programmatic path.
- **Recommendation:** Spike first — investigate whether the Krisp MCP server runs locally (reads from Krisp's local app data) vs. relying on cloud webhooks. If local, it's an M3/M6 with no networking requirement. If cloud-only, the webhook approach requires a publicly accessible receiver (not ideal for local-first). Manual .txt export via M1 is always available as a fallback.
- **Notes:** Krisp is primarily a noise cancellation tool with notetaking as a secondary feature. It works system-wide across Zoom, Teams, Google Meet, etc. The MCP server connection model is unclear from public docs — needs investigation to determine if it's a local socket or cloud relay.

#### tl;dv Meeting Recorder — _Meeting Transcripts_

🟡 **Medium — API exists and is documented, but requires the Business plan. Same webhook-only issue as Krisp (requires a reachable endpoint for real-time; polling fallback not confirmed).** · M5 · API key (Business plan required). OAuth not mentioned; API key auth. · effort **S** · 🆕 new

- **Access:** REST API + webhooks: available on Business plan. Webhook fires when transcript is ready (no polling needed). Export: TXT/Markdown/CSV from the dashboard. Multiple browser-extension transcript grabbers exist as workarounds for free-tier users. API docs at intercom.help/tldv/en/articles/11583137-api-and-webhooks.
- **Recommendation:** Build later — lower user base than Granola/Fathom/Fireflies. Add once the meetings/YYYY-MM.jsonl pattern is established.
- **Notes:** tl;dv covers Zoom, Google Meet, and Teams. 40+ language support. Grew from 30 to 5000+ integrations via Zapier/n8n. The Business plan requirement limits this to paying users — the free tier has no API access.

#### Otter.ai — _Meeting Transcripts_

🟠 **Low for individuals — API is Enterprise-only and requires contacting Otter's sales team. M1 export (TXT/DOCX/SRT) is available to all paid plans but is manual-only. No OAuth self-service path.** · M5 · Enterprise plan + account manager approval. For individuals: M1 manual export (TXT/DOCX). · effort **L** · 🆕 new

- **Access:** Enterprise API (Otter Connect API v2): contact account manager to enable. Rate limit 500 req/min on Enterprise. Export for non-Enterprise: TXT (Basic), DOCX/PDF/SRT (paid plans). Zapier integration available on Pro/Business/Enterprise for automation. No public self-serve API key for individuals.
- **Recommendation:** Icebox (API path) / Build later (M1 import). The M1 path (import Otter's .txt/.docx exports) is low-effort and available to all paid users. The Enterprise API is out of scope for a general user app. Add M1 import as part of a generic 'meeting transcript import' feature.
- **Notes:** Otter has the largest user base of any meeting notetaker among individuals and academics, making the M1 import path still valuable. SRT format is the richest export (has timestamps). The Enterprise API limitation means Trove cannot provide an automated pull for typical users — manual export is the only path.

#### Webex (Cisco) Meetings — _Meeting Transcripts_

🟢 **High for Webex users — well-documented REST API, transcript download links in VTT/TXT format, and the API now supports both Webex Assistant and Cisco AI Assistant transcripts. Webex is primarily enterprise/corporate use.** · M5 · OAuth (Webex account). meeting:read scope. Free and paid Webex accounts supported. · effort **M** · 🆕 new

- **Access:** Webex REST API: GET /v1/meetingTranscripts (list), GET /v1/meetingTranscripts/{transcriptId}/download (VTT or TXT via vttDownloadLink/txtDownloadLink). Auth: OAuth (Personal Access Token or OAuth 2.0). API docs at developer.webex.com. As of 2026 the API supports AI Assistant-generated transcripts alongside Webex Assistant transcripts.
- **Recommendation:** Build later — Webex has a smaller personal user base than Zoom. Valuable for enterprise users. Add after Zoom and Google Meet.
- **Notes:** VTT download links expire; re-fetch from the list endpoint as needed. Summaries and recordings (MP4) are also downloadable via API. Recurring series IDs must use the instance ID (not the parent series ID) to access transcripts.

#### Skype Call History — _Calls_

🟠 **Low — Skype is shut down (May 2025). The export window may have closed. The remaining value is one-time historical import for users who had Skype call history.** · M1 · Microsoft/Skype account login. Export deadline was June 2026. · effort **S** · 🆕 new

- **Access:** Skype was shut down in May 2025, with accounts migrated to Microsoft Teams. Data export was available via secure.skype.com/en/data-export (deadline June 2026). Export format: .tar archive with messages.json (includes call records). The old local macOS DB at ~/Library/Application Support/Skype/<username>/main.db (SQLite) was superseded in Skype 8+ when history moved server-side.
- **Recommendation:** Build later as a one-time historical import — low priority since Skype is defunct. The messages.json export format is well-characterized (Skyperious open-source parser as reference). Call records are present in the export alongside chat messages.
- **Notes:** Skype's shutdown means no new data will accumulate. The value is purely archival for users who had Skype call history prior to May 2025. The messages.json format includes call type, duration, and participant fields alongside chat messages — similar schema to the iMessage and Slack importers already built.

#### WhatsApp Calls (Mac Desktop app) — _Calls_

🟠 **Low — no complete local database of WhatsApp calls accessible on macOS. The iPhone app's call history is not exported via per-chat export (only messages). WhatsApp has no public API for personal data. End-to-end encryption prevents server-side access. The data-sources.md already categorizes this as M1 icebox for messages; calls are even harder.** · M1 · M1 (manual export from iPhone). Full Disk Access for macOS app cache (incomplete data only). · effort **M** · 🆕 new

- **Access:** WhatsApp Desktop on macOS stores cached data in ~/Library/Application Support/WhatsApp/ but does NOT maintain a complete local database of call history — call data primarily lives on the phone. iPhone backup path (only for local unencrypted backups): HomeDomain/Library/Application Support/CallHistory.db in some versions. Per-chat export from iPhone (Settings → Chat → Export Chat) includes call events as text lines in the .txt. No programmatic API.
- **Recommendation:** Icebox — no viable programmatic path for call history. Messages are already in the icebox. The only actionable path would be a manual note or a future iOS companion app reading from HealthKit/Contacts-adjacent logs.
- **Notes:** WhatsApp's E2E encryption and lack of API make it systematically inaccessible. The Mac Desktop app's cache is transient. Even third-party forensics tools require a local iPhone backup. This is a hard blocker that no amount of engineering can overcome within Trove's constraints.

### Calls, Voice & Meeting Transcripts — cross-cutting notes

1. UNIFIED MEETINGS VAULT PATH: All meeting-recorder services (Granola, Fathom, Fireflies, Zoom, Read.ai, Webex, Teams, tl;dv) should write to a shared meetings/<source>/YYYY-MM.jsonl schema with common fields: meeting_id, title, start_time, end_time, duration_secs, platform (zoom/meet/teams/etc.), attendees[], summary, transcript_url (local relative path to VTT/TXT), action_items[]. This mirrors the correspondence/ pattern already in Trove and allows a unified Meetings tab. 2. OAUTH PATTERN REUSE: Zoom, Google Meet, Webex, and Teams all use standard OAuth 2.0. The OAuth scaffolding built for Oura/TickTick and the in-progress Google integration covers most of this. Granola and Fathom use simpler API-key auth (same pattern as Oura PAT). Fireflies uses GraphQL with a bearer token — the same HTTP client pattern. 3. WEBHOOK vs POLL TRADEOFF: Several services (Fireflies, tl;dv, Krisp) offer webhooks as the real-time path. Trove's local-first constraint means a publicly reachable webhook endpoint is not available without a relay. The safe pattern is to use webhooks only if the user has opted into a relay, and otherwise fall back to periodic polling (e.g., on the slow tick in runner.rs). Granola and Fathom both support REST polling, which is cleaner for Trove's architecture. 4. TRANSCRIPT FORMAT NORMALIZATION: Services use VTT (Zoom, Teams, Webex), JSON with sentence arrays (Fireflies), markdown (Granola, Fathom). Trove should store the raw format alongside a normalized plain-text transcript in the JSONL record for AI/search use, with the rich format (VTT/JSON) preserved as a sidecar file in meetings/<source>/transcripts/<id>.vtt. 5. LOCAL SOURCES ALREADY UNDER FDA: Voice Memos (CloudRecordings.db) and voicemail (MobileSync backup) both require FDA, which troved already holds. These should be wired into the existing troved poll loop on the slow tick, alongside CallHistoryDB. 6. PLAN-GATING PATTERN: Granola (Business for raw transcripts), Otter (Enterprise for API), tl;dv (Business for API) all gate the API behind paid plans. Trove should handle graceful degradation: if the API returns 403/plan errors, surface a UI hint about the required plan rather than silently failing. This mirrors the existing disabled-controls-need-affordance principle from CLAUDE.md. 7. VOICE MEMOS tsrp ATOM: The tsrp embedded transcript approach (macOS 15+) is a zero-dependency win — no Whisper model needed for transcripts on modern systems. The parsing logic is a ~50-line binary scan for the JSON sentinel; reference implementation at github.com/pedramamini/voice-memos gist and github.com/jwulff/apple-voice-memo-mcp. This should be prioritized over bundling a Whisper model for Voice Memos. 8. GOOGLE VOICE TAKEOUT: The Takeout HTML format is the only path for Google Voice and is straightforward — community parsers (voice2json, NeighborGeek gist) provide reference implementations. This is a rare case where M1 is both the only option AND low friction, making it worth building promptly. 9. SKYPE ARCHIVAL URGENCY: Skype's data export deadline was June 2026. If any users still need to export their Skype history, this is time-critical. The importer should be treated as a one-shot historical migration like Apple Health export.zip.

---

## People, Contacts & Relationship Graph

The "who" backbone of Trove crosses two distinct layers: raw contact records (names, phones, emails, birthdays, social handles) and derived relationship graphs (who you communicate with most, extracted from iMessage, email, calendar, and call history already collected). macOS Contacts via CNContactStore (TCC-gated, but a clean one-time prompt) is the highest-value single pull — it consolidates iCloud, Exchange, Google, CardDAV, and local contacts in one API call and delivers structured vCards including birthdays and anniversaries. Cloud contact services (Google Contacts via People API, iCloud via CardDAV) add accounts that may not sync locally. Personal CRM tools (Monica, Dex, Clay/Mesh) are niche but straightforward M1/M5 imports for users who maintain one. The largest untapped opportunity is derived interaction scoring: Trove already has iMessage, email, calendar, and call history in the vault — a pure read-time pass can build a rich relationship graph with zero new permissions or API calls. Entity resolution (deduplicating a person across phone, email, iMessage handle, LinkedIn URL) is the essential cross-cutting infrastructure that makes the whole domain coherent.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| macOS Contacts (CNContactStore / Contacts.framework) | System Contacts | M4 | TCC: kTCCServiceAddressBook (standard system dialog, one-time user grant). SQLite direct-read alternative needs Full Disk Access instead. | M | 🟢 High — standard TCC API, well-documented. Trove already has the EventKit TCC bridge pattern (src/eventkit.rs); contacts follow the same shape. Using CNContactStore (not raw SQLite) is strongly preferred: it respects the TCC grant, handles multi-account federation (iCloud + Exchange + Google + LDAP + CardDAV all appear in one store), and is the privacy-respecting path. | 📋 planned |
| Interaction Graph (derived from vault sources) | Relationship Intelligence | M3 | None beyond permissions already granted for iMessage/email/calendar collectors. | M | 🟢 High — all source data already exists in the vault as JSONL/CSV. Zero new permissions. This is the highest-signal relationship intelligence in Trove and costs nothing new. | 🆕 new |
| Google Contacts (People API) | Cloud Contacts | M5 | OAuth token with `contacts.readonly` scope. Trove already has OAuth plumbing (src/sync/oauth.rs). | M | 🟢 High — well-documented REST API, same OAuth infrastructure already used for TickTick/Oura. People API replaced deprecated Contacts API in 2022 and is stable. Contacts.readonly is a non-sensitive scope (no verification required). Rate limits: 90 requests/minute per user, well within polling needs. | 🆕 new |
| iCloud Contacts (CardDAV) | Cloud Contacts | M5 | Apple ID credentials + app-specific password (not OAuth). No TCC needed for the CardDAV path. | L | 🟡 Medium — CardDAV works (vdirsyncer is regularly tested against iCloud and confirmed functional). However: (1) Apple does not publish official CardDAV documentation for third-party apps; (2) app-specific passwords require the user to generate one manually in appleid.apple.com; (3) the iCloud CardDAV API is the same data as macOS Contacts (CNContactStore) if the user has Contacts sync enabled. So for most users, CNContactStore is strictly better: same data, official API, no credential management. | 🆕 new |
| LinkedIn Connections Export | Professional Network | M1 | None (user-initiated export from LinkedIn.com). No API key or OAuth needed. | S | 🟢 High — LinkedIn has provided this export consistently for years, it is still available in 2026, and the format is stable. The CSV is straightforward to parse. Primary limitation: Email addresses are often missing (10-20% of connections have email visible); phone numbers are never included. | 🆕 new |
| Monica HQ (Personal CRM) | Personal CRM | M5 | Monica API token (generated in Settings → API). No OAuth — simple Bearer token. | S | 🟢 High — Monica's REST API is stable, well-documented at monicahq.com/api, and actively maintained (open-source PHP Laravel, active GitHub). Niche but dedicated user base. Both cloud and self-hosted paths use the same API contract. | 🆕 new |
| Dex Personal CRM | Personal CRM | M5 | Dex API Bearer token (Professional plan required). Or M1 via CSV export from Settings. | S | 🟡 Medium — API exists but is paywalled (Professional plan). CSV export is free. The niche user base (LinkedIn-integrated personal CRM) means this is valuable for the specific users who rely on Dex for relationship tracking. Primary risk: Dex is a VC-funded startup; API stability is not guaranteed long-term. | 🆕 new |
| Clay / Mesh (Personal CRM) | Personal CRM | M1 | None (user-initiated CSV export from clay.earth). | S | 🟡 Medium — CSV export works but there is no stable public API for personal accounts. Clay is VC-funded; the product has rebranded and pivoted. Primary value is enriched contact profiles (job changes, news mentions) that go beyond raw address book data. | 🆕 new |
| vCard / VCF File Import (generic) | Universal Contact Format | M1 | None (user drops the file). | S | 🟢 High — vCard is the universal contact interchange format. Every contact service supports export to .vcf. This is the lowest-friction path for any source that doesn't have a direct connector yet. | 🆕 new |
| Birthdays & Anniversary Calendar Derivation | Life Events | M4 | TCC: kTCCServiceAddressBook (same as contacts pull) for path 1. kTCCServiceCalendar (already granted) for path 2. | S | 🟢 High — birthday and anniversary fields are part of the standard CNContact record. The EventKit Birthdays calendar is a synthetic view that is already available once calendar access is granted. Zero additional permission needed beyond what is already built. | 🆕 new |
| Google Contacts Takeout (M1 fallback) | Cloud Contacts | M1 | None (user-initiated export from takeout.google.com). | S | 🟢 High — Google Takeout for contacts has been stable for years. The vCard export is standard RFC format. This is the zero-auth fallback when the user cannot or will not set up OAuth for the People API connector. | 🆕 new |
| Notion / Airtable as Personal Contact DB | Personal CRM | M5 | Notion integration secret (created at notion.so/my-integrations, then shared with the specific database). Airtable PAT with `data.records:read` scope. | M | 🟡 Medium — both APIs are stable and well-documented. The challenge: both are highly schema-flexible (users design their own contact database), so Trove needs a field-mapping configuration step rather than a fixed schema import. Many users use Notion/Airtable as a personal CRM but with widely varying column structures. | 🆕 new |
| Entity Resolution Layer (cross-source identity merging) | Identity Infrastructure | M6 | None beyond what each individual source already requires. | L | 🟢 High — all needed data is local. The computation is deterministic and re-runnable. Challenge is the algorithm quality: simple exact-match on normalized phone/email handles 80% of cases; fuzzy name matching for the remainder introduces false-positive risk. | 🆕 new |

### Detail

#### macOS Contacts (CNContactStore / Contacts.framework) — _System Contacts_

🟢 **High — standard TCC API, well-documented. Trove already has the EventKit TCC bridge pattern (src/eventkit.rs); contacts follow the same shape. Using CNContactStore (not raw SQLite) is strongly preferred: it respects the TCC grant, handles multi-account federation (iCloud + Exchange + Google + LDAP + CardDAV all appear in one store), and is the privacy-respecting path.** · M4 · TCC: kTCCServiceAddressBook (standard system dialog, one-time user grant). SQLite direct-read alternative needs Full Disk Access instead. · effort **M** · 📋 planned

- **Access:** Apple Contacts.framework via CNContactStore. TCC permission: `kTCCServiceAddressBook`. Requires `NSContactsUsageDescription` in Info.plist. Rust implementation: thin Swift shim (or objc2 crate) calling CNContactStore, returning serialized data over FFI; alternatively a short native-messaging host binary. Alternatively: direct SQLite read of `~/Library/Application Support/AddressBook/AddressBook-v22.abcddb` — 34 tables, key tables: ZABCDRECORD (name, org, birthday), ZABCDPHONENUMBER, ZABCDEMAILADDRESS, ZABCDPOSTALADDRESS, ZABCDSOCIALPROFILE, ZABCDURLADDRESS, ZABCDCONTACTDATE (anniversaries), ZABCDNOTE.
- **Recommendation:** Build now — this is the anchor for the whole domain. One TCC prompt yields every contact the user has across all synced accounts, plus birthdays/anniversaries/social handles. Outputs: `contacts/contacts.jsonl` (one person per line, normalized), `contacts/index.md` (human summary). The CNContactStore API already handles dedup across accounts; Trove just consumes the result.
- **Notes:** CNContactBirthdayKey and CNContactDatesKey expose birthday and all date fields (anniversaries, other). ZABCDCONTACTDATE in the raw SQLite also stores all dates. The CNContactStore approach is preferred over direct SQLite because: (1) the SQLite path needs Full Disk Access (higher permission bar) vs. the Contacts-specific TCC for the framework path; (2) CNContactStore does cross-account dedup natively; (3) it is future-proof against schema changes. The `objc2` and `objc2-contacts` crates provide zero-overhead Rust bindings to Contacts.framework — no Swift shim needed. troved can poll every 6h or react to a file-system change notification on the AddressBook directory.

#### Interaction Graph (derived from vault sources) — _Relationship Intelligence_

🟢 **High — all source data already exists in the vault as JSONL/CSV. Zero new permissions. This is the highest-signal relationship intelligence in Trove and costs nothing new.** · M3 · None beyond permissions already granted for iMessage/email/calendar collectors. · effort **M** · 🆕 new

- **Access:** Pure read-time computation over data Trove already holds: `correspondence/imessage/`, `correspondence/email/`, `calendar/events/`, call history (CallHistoryDB already in vault). Extract all participant addresses (phone, email, iMessage handle) from each source, count interactions per participant, weight by recency, and produce a ranked list of contacts by interaction frequency. Merge with contacts store to resolve identities.
- **Recommendation:** Build now — extremely high value for zero incremental permission cost. Produces a `contacts/interaction-graph.jsonl` (one person per line: normalized_id, display_name, interaction_count, last_interaction_date, sources[]) and a `contacts/interaction-weekly.jsonl` trend. The graph also powers identity resolution: a phone number seen in iMessage, an email in correspondence, and a calendar attendee email can all map to a single CNContact record.
- **Notes:** Key normalization rules: strip E.164 country codes for phone matching, lowercase+trim emails, strip mailto: prefixes from iMessage handles that are emails. Recency decay: a simple exponential half-life of 30 days per interaction produces a sensible 'relationship strength' score. This is the foundation that makes the contacts store useful rather than just a flat list. Recommended data structure: a `HashMap<NormalizedKey, PersonRecord>` built in `contacts.rs`, where NormalizedKey is an enum over (phone, email, handle). The computation is fast enough (vault sizes are small) to run on every vault open without a persistent index.

#### Google Contacts (People API) — _Cloud Contacts_

🟢 **High — well-documented REST API, same OAuth infrastructure already used for TickTick/Oura. People API replaced deprecated Contacts API in 2022 and is stable. Contacts.readonly is a non-sensitive scope (no verification required). Rate limits: 90 requests/minute per user, well within polling needs.** · M5 · OAuth token with `contacts.readonly` scope. Trove already has OAuth plumbing (src/sync/oauth.rs). · effort **M** · 🆕 new

- **Access:** Google People API v1. Endpoint: `https://people.googleapis.com/v1/people/me/connections?personFields=names,emailAddresses,phoneNumbers,birthdays,addresses,organizations,urls,biographies,photos`. Also `people.googleapis.com/v1/otherContacts` for auto-saved contacts. OAuth2 scope: `https://www.googleapis.com/auth/contacts.readonly`. Google Takeout alternative: contacts.google.com → Export → Google CSV or vCard (.vcf).
- **Recommendation:** Build now — many users have Google as their primary contact source and it may not sync to macOS Contacts if the user hasn't configured Google account in System Settings. Outputs merged into same `contacts/` vault folder with `source: google_contacts`. Incremental sync via `syncToken` (People API supports change tokens). The Google integration worktree is already noted as in-progress for Calendar; contacts connector can ride the same OAuth flow.
- **Notes:** Google Takeout exports contacts as vCard 3.0 (.vcf) or Google CSV. The vCard format is parseable by Rust crates `vcard_parser` or `calcard` (stalwartlabs). People API returns `etag` and `resourceName` (format: `people/c<id>`) for stable identity across syncs. `otherContacts` (auto-saved from email/chat) is available at a separate endpoint and is worth fetching — it often contains people you've emailed but not formally added.

#### iCloud Contacts (CardDAV) — _Cloud Contacts_

🟡 **Medium — CardDAV works (vdirsyncer is regularly tested against iCloud and confirmed functional). However: (1) Apple does not publish official CardDAV documentation for third-party apps; (2) app-specific passwords require the user to generate one manually in appleid.apple.com; (3) the iCloud CardDAV API is the same data as macOS Contacts (CNContactStore) if the user has Contacts sync enabled. So for most users, CNContactStore is strictly better: same data, official API, no credential management.** · M5 · Apple ID credentials + app-specific password (not OAuth). No TCC needed for the CardDAV path. · effort **L** · 🆕 new

- **Access:** CardDAV server: `https://contacts.icloud.com/`. Requires Apple ID + app-specific password (iCloud 2FA does not work over DAV). PROPFIND to discover address book collections, then GET individual vCard resources. The Rust `mini_dav` or `carddav_client` approach, or wrapping `vdirsyncer` logic. Alternatively: iCloud.com → Contacts → select all → Export vCard (.vcf file, M1).
- **Recommendation:** Spike first, then decide based on user need — primarily valuable for users who do NOT have macOS Contacts sync enabled for iCloud, or who want to sync without granting the app-wide TCC permission. Given CNContactStore covers the same data for most users, this is lower priority. The vCard M1 import path (drop the .vcf export) is a free fallback requiring zero auth.
- **Notes:** iCloud CardDAV quirks: collection names must be ≥ some minimum length; collections should be created from Apple clients. Write access via CardDAV has had 403 issues reported (issue #1145 in vdirsyncer). The programmatic path here is read-only for Trove, so write issues don't apply. If implemented, the `calcard` Rust crate (stalwartlabs/calcard) handles both iCalendar and vCard parsing.

#### LinkedIn Connections Export — _Professional Network_

🟢 **High — LinkedIn has provided this export consistently for years, it is still available in 2026, and the format is stable. The CSV is straightforward to parse. Primary limitation: Email addresses are often missing (10-20% of connections have email visible); phone numbers are never included.** · M1 · None (user-initiated export from LinkedIn.com). No API key or OAuth needed. · effort **S** · 🆕 new

- **Access:** LinkedIn.com → Settings & Privacy → Data Privacy → Get a copy of your data → Connections. Produces `Connections.csv` with fields: First Name, Last Name, Email Address, Company, Position, Connected On. Download available within 10-24 minutes of request, link sent by email. File format: CSV, UTF-8.
- **Recommendation:** Build now — very low effort (S), and LinkedIn is where most professional contacts originate. The Connected On date enables relationship timeline analysis. Map to the contacts schema: `source: linkedin`, identity fields: first+last name + company + (email if present). The import should be re-runnable (idempotent by LinkedIn profile URL or name+company composite key). Note: LinkedIn's API (OAuth) is not viable for personal data export — it is restricted to approved partners and does not expose personal connection lists programmatically.
- **Notes:** LinkedIn does not offer a public API for personal connections. The v1 API (deprecated 2015) and v2 API (2019+) both require OAuth app approval and do not include personal connection data. The M1 export is the only realistic path. Trove should provide a 'drag your Connections.csv here' import card in the Contacts section. The export can be automated by prompting the user to re-export periodically (quarterly is sufficient for most users).

#### Monica HQ (Personal CRM) — _Personal CRM_

🟢 **High — Monica's REST API is stable, well-documented at monicahq.com/api, and actively maintained (open-source PHP Laravel, active GitHub). Niche but dedicated user base. Both cloud and self-hosted paths use the same API contract.** · M5 · Monica API token (generated in Settings → API). No OAuth — simple Bearer token. · effort **S** · 🆕 new

- **Access:** Two paths: (1) Monica Cloud (monicahq.com): REST API at `https://app.monicahq.com/api` with Bearer token auth. Endpoints: GET /contacts (paginated), GET /contacts/{id}, GET /activities, GET /notes, GET /reminders. (2) Self-hosted Monica: same REST API, configurable base URL via `MONICA_BASE_URL`. JSON export: Settings → Export Data → JSON. Also supports vCard export per contact.
- **Recommendation:** Build later — the user population with Monica is small but the integration is trivially easy (Bearer token + JSON REST). The Monica MCP server already exists (jacob-stokes/monica-crm-mcp), which could serve as an M6 fallback if a direct connector isn't prioritized. Primary value: notes, last-contact dates, relationship context that don't exist in the address book.
- **Notes:** Monica exports: full JSON dump includes contacts, activities, notes, reminders, relationships, gifts, conversations. The JSON export is the best path for a one-time migration; the API is better for ongoing sync. Monica v3 (Chandler) has a significantly updated schema vs v2 — verify endpoint compatibility. Monica cloud pricing: free tier limited to 10 contacts (cloud), unlimited on self-hosted. Key unique fields: `last_called`, `last_talked_to`, notes timeline, relationship labels.

#### Dex Personal CRM — _Personal CRM_

🟡 **Medium — API exists but is paywalled (Professional plan). CSV export is free. The niche user base (LinkedIn-integrated personal CRM) means this is valuable for the specific users who rely on Dex for relationship tracking. Primary risk: Dex is a VC-funded startup; API stability is not guaranteed long-term.** · M5 · Dex API Bearer token (Professional plan required). Or M1 via CSV export from Settings. · effort **S** · 🆕 new

- **Access:** Dex API at `https://api.getdex.com/api/rest/contacts` with Bearer token (available on Professional plan at $20/month). CSV export: Settings → Export Data. Import/export uses standard CSV with header matching on first_name/last_name/email/company/tags. No webhook or real-time push — polling only.
- **Recommendation:** Build later — low effort once the contacts import infrastructure exists. The M1 CSV export is the free fallback requiring zero API work. The unique Dex value is LinkedIn enrichment data and interaction reminders that don't exist in raw contacts.
- **Notes:** Dex integrates with LinkedIn, Gmail, Calendar, iMessage, and Twitter to auto-log interactions. The exported CSV includes tags, notes, and last-contact dates. The free tier has no CSV export — must upgrade to export, which is a significant friction point. A user on the free tier can only use the API (paywalled) or manually curate a CSV.

#### Clay / Mesh (Personal CRM) — _Personal CRM_

🟡 **Medium — CSV export works but there is no stable public API for personal accounts. Clay is VC-funded; the product has rebranded and pivoted. Primary value is enriched contact profiles (job changes, news mentions) that go beyond raw address book data.** · M1 · None (user-initiated CSV export from clay.earth). · effort **S** · 🆕 new

- **Access:** Clay (now partially rebranded as 'Mesh' at clay.earth for personal use) does not offer a public REST API for personal-tier users. Primary data access path: CSV export from the Clay dashboard. Clay enriches contacts by pulling from email, calendar, LinkedIn, Twitter, iMessage — but the enriched data export is the only Trove-accessible artifact. Note: clay.com is a separate B2B data enrichment product; clay.earth is the personal CRM.
- **Recommendation:** Icebox — too few users, no API, and ongoing pivot/rebrand uncertainty. Revisit if an API becomes available. The M1 CSV import path can be added generically under a 'Personal CRM CSV import' option that handles Clay/Dex/Monica CSV exports in one flow.
- **Notes:** Clay's free personal tier includes up to 1,000 contacts, imports from email/calendar/LinkedIn/Twitter/iMessage. The Pro tier ($10/month annual) adds unlimited contacts and CSV import. The enrichment data (job titles, company updates) is the differentiator but is Clay-computed and not independently verifiable — treat as supplementary metadata, not authoritative.

#### vCard / VCF File Import (generic) — _Universal Contact Format_

🟢 **High — vCard is the universal contact interchange format. Every contact service supports export to .vcf. This is the lowest-friction path for any source that doesn't have a direct connector yet.** · M1 · None (user drops the file). · effort **S** · 🆕 new

- **Access:** Any .vcf file drop. Sources: iCloud.com → Contacts → Export vCard; Google Contacts → Export → vCard; Outlook → File → Export → vCard; CardDAV server direct download; exchange with any app that exports contacts. Format: RFC 6350 vCard 4.0 or 3.0. Rust parsing: `vcard_parser` crate (RFC 6350 compliant) or `calcard` (stalwartlabs, also handles JSContact).
- **Recommendation:** Build now — generic vCard import is the universal fallback for any contact source and takes minimal effort given the Rust library ecosystem. One importer handles iCloud export, Google Contacts export, Outlook export, and CardDAV server exports simultaneously. Outputs to `contacts/contacts.jsonl` with `source: vcard_import`. Idempotent by UID field (vCard 4.0 UUID) or name+email composite key.
- **Notes:** vCard 4.0 (RFC 6350) adds: BDAY (birthday), ANNIVERSARY, GENDER, KIND, RELATED (relationships), IMPP (IM handles), URL. The `calcard` crate from Stalwart Labs supports vCard 4.0 and JSContact conversion, and is actively maintained. For large exports (10k+ contacts) the import should be streamed rather than fully loaded into memory. Profile photos in vCards are base64-encoded JPEG/PNG inside the PHOTO field — strip or extract separately.

#### Birthdays & Anniversary Calendar Derivation — _Life Events_

🟢 **High — birthday and anniversary fields are part of the standard CNContact record. The EventKit Birthdays calendar is a synthetic view that is already available once calendar access is granted. Zero additional permission needed beyond what is already built.** · M4 · TCC: kTCCServiceAddressBook (same as contacts pull) for path 1. kTCCServiceCalendar (already granted) for path 2. · effort **S** · 🆕 new

- **Access:** Two paths: (1) CNContactStore: CNContactBirthdayKey (NSDateComponents, Gregorian) and CNContactNonGregorianBirthdayKey, plus CNContactDatesKey (array of labeled dates including anniversaries). These are available in the same contacts pull — no additional permission. (2) Apple Calendar: macOS creates a synthetic read-only 'Birthdays' calendar from contacts' birthday fields, accessible via EventKit (already built in Trove's calendar.rs).
- **Recommendation:** Build now (as part of the CNContactStore collector) — trivial incremental work once contacts pull is built. Output: include `birthday` and `anniversaries[]` fields in the contacts JSONL record. Optionally produce a `contacts/upcoming-events.jsonl` (birthdays/anniversaries within N days) for the UI notification layer.
- **Notes:** CNContactBirthdayKey returns an NSDateComponents with year optionally nil (some users store month/day only). The date components approach handles this gracefully — year-less birthdays are stored as MM-DD strings. CNContactDatesKey is an array of CNLabeledValue<NSDateComponents> with labels like `_$!<Anniversary>!$_` or custom strings. The Birthdays calendar in EventKit (accessed via EKCalendar where type == .birthday) is read-only and cannot be used to add events, but it correctly surfaces all contact birthdays for the calendar view.

#### Google Contacts Takeout (M1 fallback) — _Cloud Contacts_

🟢 **High — Google Takeout for contacts has been stable for years. The vCard export is standard RFC format. This is the zero-auth fallback when the user cannot or will not set up OAuth for the People API connector.** · M1 · None (user-initiated export from takeout.google.com). · effort **S** · 🆕 new

- **Access:** takeout.google.com → select Contacts → Download. Produces a .zip containing `contacts.vcf` (all contacts as vCard 3.0) or `google.csv` (Google-specific CSV with extended fields). The vCard export is parseable by the generic vCard importer. Google Contacts web UI alternative: contacts.google.com → Export → vCard or Google CSV.
- **Recommendation:** Build now (free, handled by generic vCard importer) — once vCard import exists, Google Contacts Takeout import is automatic with zero additional code. Just document the import path.
- **Notes:** Google CSV export includes additional fields not in standard vCard: Group Membership (contact groups/labels), Notes (free text), custom fields. The `google.csv` format has non-standard column names and is harder to parse than vCard — prefer the vCard export. Google exports contacts.vcf and also separates 'Other contacts' (auto-saved from Gmail) into a separate file if selected.

#### Notion / Airtable as Personal Contact DB — _Personal CRM_

🟡 **Medium — both APIs are stable and well-documented. The challenge: both are highly schema-flexible (users design their own contact database), so Trove needs a field-mapping configuration step rather than a fixed schema import. Many users use Notion/Airtable as a personal CRM but with widely varying column structures.** · M5 · Notion integration secret (created at notion.so/my-integrations, then shared with the specific database). Airtable PAT with `data.records:read` scope. · effort **M** · 🆕 new

- **Access:** Notion REST API (v1): `https://api.notion.com/v1/databases/{database_id}/query` with Bearer token (integration secret). Query the user's contact database. Export: Notion UI → Export → CSV. Airtable REST API (v0): `https://api.airtable.com/v0/{base_id}/{table_name}` with Personal Access Token (PAT). Legacy API keys disabled Feb 2024. Export: Airtable UI → Download CSV.
- **Recommendation:** Build later — the user population that maintains Notion/Airtable contact databases is meaningful but the schema flexibility makes a generic connector tricky. The M1 CSV export path (both support CSV download) is a reasonable first step. An API connector that asks the user to map columns to the Trove contacts schema would be the proper implementation.
- **Notes:** Notion API returns pages as structured objects with rich text, title, select, date, and relation property types. A contact 'page' in Notion has no fixed schema — Trove would need to prompt the user to identify which property is 'name', 'email', etc. Airtable's Contact Import Extension can pull contacts directly from iOS/macOS Contacts into Airtable, meaning some Airtable contact databases are actually mirrors of the address book. In that case, the CNContactStore pull is strictly better. Prioritize if user demand surfaces.

#### Entity Resolution Layer (cross-source identity merging) — _Identity Infrastructure_

🟢 **High — all needed data is local. The computation is deterministic and re-runnable. Challenge is the algorithm quality: simple exact-match on normalized phone/email handles 80% of cases; fuzzy name matching for the remainder introduces false-positive risk.** · M6 · None beyond what each individual source already requires. · effort **L** · 🆕 new

- **Access:** Pure in-process computation. Input: all contact records from all sources (CNContacts, Google, LinkedIn, Monica, etc.) + all interaction graph addresses (phone numbers, emails, iMessage handles from correspondence/). Output: a unified `contacts/persons.jsonl` where each record is a merged person with all known identities, aliases, and source references. No external service needed.
- **Recommendation:** Spike first — define the normalized identity schema and implement the exact-match layer (phone E.164 normalization, email lowercase+trim) as the foundation. Ship the exact-match resolver with the first contacts collector; defer fuzzy name matching to a later pass. The entity resolver is the infrastructure that makes the relationship graph useful: it turns 'j.doe@gmail.com' in your email, '+14155551234' in iMessage, and 'John Doe' in your address book into one person.
- **Notes:** Recommended implementation: `contacts/persons.jsonl` as the output, where each line is `{person_id: uuid, canonical_name, identities: [{type: email|phone|imessage_handle|linkedin_url|apple_contact_id, value: ..., source: ...}], merged_from: [...]}`. The person_id is stable across re-runs (derived from the primary identity or assigned once and stored). The Python `recordlinkage` library and `nomenklatura` OSS toolkit are references; Trove should implement a simpler Rust-native version tailored to contact identifiers. Key normalization rules: E.164 phone parsing via the `phonenumber` Rust crate; email lowercased + trimmed; iMessage handles that look like emails are treated as emails for matching.

### People, Contacts & Relationship Graph — cross-cutting notes

1. SHARED INFRASTRUCTURE: A single `contacts.rs` module in trove-core should own the contacts schema, the JSONL write path, and the entity resolver. All source-specific connectors (CNContactStore, Google People API, LinkedIn CSV, vCard importer, Monica API) write through this one module so deduplication and normalization happen in one place.

2. VAULT LAYOUT: Recommend `contacts/` folder with: `contacts.jsonl` (raw per-source records, one per line, with `source` field), `persons.jsonl` (entity-resolved merged records), `interaction-graph.jsonl` (derived from existing correspondence data), `index.md` (human-readable summary). Keep raw records separate from resolved persons so the resolver is re-runnable without data loss.

3. PERMISSION SEQUENCING: The CNContactStore (kTCCServiceAddressBook) TCC prompt should be batched with the calendar permission prompt — both are low-friction and users expect a contacts+calendar bundle. The existing EventKit TCC bridge pattern in src/eventkit.rs is the exact template to follow for contacts.

4. RUST LIBRARIES: `objc2-contacts` crate for CNContactStore FFI (zero-overhead Objective-C bindings, no Swift shim needed); `vcard_parser` or `calcard` (stalwartlabs) for vCard parsing; `phonenumber` crate for E.164 normalization in entity resolution.

5. INTERACTION GRAPH AS FORCE MULTIPLIER: The derived interaction graph (built from already-collected iMessage, email, calendar, call data) delivers the highest relationship intelligence for zero new permissions. It should be the first thing built after the CNContactStore pull, because it makes the flat contacts list queryable by 'who matters most'.

6. LINKEDIN API IS BLOCKED: LinkedIn provides no public API for personal connection data. The M1 CSV export is the only viable path. Do not invest engineering time in a LinkedIn API connector.

7. PERSONAL CRM PRIORITY ORDER: Monica (open-source, REST API, niche but dedicated) > Dex (API paywalled, CSV free) > Clay/Mesh (no personal API, CSV only) > Notion/Airtable (schema-flexible, needs user mapping step). All of these are secondary to the CNContactStore pull and interaction graph.

8. BIRTHDAY/ANNIVERSARY DATA: Available for free from the CNContactStore pull (CNContactBirthdayKey, CNContactDatesKey) — no separate collector needed. Include these fields in the contacts JSONL and surface them in the UI as life events alongside calendar events.

9. ENTITY RESOLUTION RISK: Fuzzy name matching (soundex, edit distance) introduces false-positive merge risk — e.g. 'John Smith' at two companies wrongly merged. The safe default is exact-match-only on phone/email identifiers, with fuzzy matching as an opt-in or user-confirmed step. Never auto-merge without a shared exact identifier.

---

## Health: Wearables & Biometrics

This domain covers body-signal hardware, their cloud APIs, and local-export pathways. The single most important practical reality: the vast majority of consumer wearables (Garmin, Fitbit/Google, Polar, Withings, Coros, Suunto, Amazfit, Omron) sync to Apple Health on iOS, so the already-built Apple Health export.zip import already captures a large fraction of wearable data for iPhone users. The independent value of direct vendor API pulls is highest for: (a) fields the vendor does NOT push to Apple Health (e.g. Oura's detailed HRV 5-min intervals, WHOOP recovery/strain, Garmin's FIT-file aerobic training analytics, Withings ECG/nerve metrics, sleep-mat respiratory data), (b) users who do not own an iPhone, and (c) richer historical backfill before HealthKit sync was enabled. Feasibility is generally High for official OAuth APIs (Oura, WHOOP, Withings, Garmin export, Polar, Dexcom, Strava) and Medium for devices that lack public APIs (Eight Sleep, Amazfit/Zepp, Renpho). Trove's standalone constraint is fully compatible with all OAuth pulls and one-shot export imports; no source in this domain requires a running third-party process.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Apple Health export.zip (deep parse) | Platform Health Hub | M1; M2 for auto-export watch folder | none (user-initiated) / iCloud Drive read for M2 | S | 🟢 High — already built; deepen by adding auto-export watch-folder path and parsing remaining data types not yet extracted | ✅ built |
| Oura Ring | Smart Ring | M5 | OAuth | S | 🟢 High — official public API, self-service OAuth app registration at cloud.ouraring.com/oauth/applications | ✅ built |
| WHOOP | Fitness/Recovery Band | M5; M1 fallback (CSV export) | OAuth | M | 🟢 High — well-documented public API, v2 active in 2026 (v1 webhooks removed). Self-service developer registration with active WHOOP subscription required. | 🆕 new |
| Withings (Scales, Sleep Mat, Blood Pressure, Trackers, ScanWatch) | Smart Scale / Multi-Device | M5 | OAuth | M | 🟢 High — well-documented, self-service, no commercial approval needed for personal/app use. Withings is unique in offering scale-based body composition + ECG + blood pressure + sleep mat + thermometer all under one API. | 🆕 new |
| Garmin Connect (Garmin watches/GPS devices) | GPS Watch / Sports Watch | M5; M1 strong fallback | OAuth (official API) / account credentials (export) | L | 🟡 Medium — official API is business-use only and approval-gated; not reliably self-service for a personal app. Full data export (M1) is excellent for historical data and completely self-service. Unofficial API (used by GarminDB, garmin-connect-export scripts) works by reusing the browser session but is fragile and TOS-gray. | 🆕 new |
| Fitbit / Google Health API | Fitness Tracker | M5 | OAuth (Google Cloud project required) | M | 🟡 Medium — legitimate API but Google Cloud setup overhead, CASA review for production at scale (not an issue for personal app up to 100 users), and the migration away from Fitbit API must complete by September 2026. Self-service for personal/small scale. | 🆕 new |
| Dexcom CGM (G6, G7, ONE, ONE+) | Continuous Glucose Monitor | M5 | OAuth | M | 🟢 High for personal use — developer.dexcom.com is self-service up to 5 authorized users (more than enough for personal app). G6, G7, ONE, ONE+ supported. Note: Dexcom API v2 endpoints shut down May 2026; v3 is the current target. | 🆕 new |
| Polar (watches and HR monitors) | Sports Watch / HR Monitor | M5; M1 fallback | OAuth | M | 🟢 High — public API, self-service registration (just a Polar Flow account needed), no commercial approval required. Well-documented AccessLink v4. | 🆕 new |
| Abbott FreeStyle Libre / LibreView (CGM) | Continuous Glucose Monitor | M1 (CSV export); M5 via aggregator; M5 unofficial fallback | account credentials (unofficial API) / none (CSV export) | L | 🟡 Medium — no official direct API; unofficial LibreView API is fragile and TOS-gray. CSV export is reliable but manual. The Apple Health sync via LibreLinkUp covers basic glucose values so Apple Health import already captures this for Libre users. | 🆕 new |
| Strava (workout activities with GPS) | Workout/Activity Platform | M5 | OAuth | M | 🟢 High — public self-service OAuth, extremely well-documented, enormous user base. Strava often aggregates workouts from Garmin, Apple Watch, Suunto, Polar, Wahoo — so it can be the single pull for multi-device athletes. | 🆕 new |
| Withings Smart Scale (standalone — Withings users without other Withings devices) | Smart Scale / Body Composition | M5 | OAuth | S | 🟢 High — same API as main Withings entry; separate entry here because many users own only a Withings scale without other Withings devices. | 🆕 new |
| Omron Blood Pressure Monitors | Blood Pressure / Cardiac | M5; M3-adjacent (libomron direct device read) | OAuth; none for libomron (USB access) | M | 🟡 Medium — OMRON Connect Create requires contacting them for developer onboarding (not pure self-service). libomron (github.com/openyou/libomron) provides direct device access but coverage varies by model and the project is aging. OMRON syncs BP to Apple Health via the official app. | 🆕 new |
| Coros (GPS sports watches) | GPS Watch / Sports Watch | M5 | OAuth (requires application approval) | L | 🟡 Medium — application-gated, not self-service. Unclear if approved for individual developers vs. business entities only. The COROS MCP server (May 2026) is an interesting M6-adjacent path but requires the user to be running it. | 🆕 new |
| Suunto (GPS watches) | GPS Watch / Sports Watch | M5 | OAuth (application approval required) | L | 🟡 Medium — public documentation exists but 'companies and organizations' framing suggests business-use bias; personal developer access unclear. The SuuntoPlus opening (March 2026) is for watch-face/app developers, not necessarily the Cloud API. Suunto syncs to Apple Health. | 🆕 new |
| Eight Sleep Pod (smart mattress) | Sleep Tech / Smart Mattress | M5 (unofficial cloud API); M3-adjacent (local Free Sleep SQLite) | account credentials (unofficial cloud); SSH to device (Free Sleep) | L | 🟡 Medium — unofficial cloud API works but is TOS-gray and fragile at API changes. Free Sleep local path is technically excellent (local SQLite, full biometric fidelity) but requires user to root their Pod, which is a significant barrier. Eight Sleep syncs some sleep data to Apple Health but biometric detail (HR, HRV, breath rate time series, room temperature) is not in Apple Health. | 🆕 new |
| Amazfit / Zepp Health (Xiaomi smartwatches) | Smartwatch | M1 (GPX export per workout); M5 unofficial cloud | account credentials (unofficial) | XL | 🟠 Low — no official API, unofficial endpoints frequently change, no Rust library. Apple Health sync via Zepp app covers most data points for Apple Health users. | 🆕 new |
| Ultrahuman Ring AIR | Smart Ring | M5 | OAuth (application approval required) | L | 🟡 Medium — gated developer program, not self-service. Growing ring segment; Ultrahuman is unique in combining ring biometrics with optional CGM in one API. Data does sync to Apple Health partially. | 🆕 new |
| Nightscout (self-hosted CGM aggregator) | Continuous Glucose Monitor / Open Platform | M5 (pull from user's own Nightscout instance) | API secret / JWT (user provides their own Nightscout URL + token) | S | 🟢 High — clean REST API, fully open-source, no approval needed. Relevant only for users who self-host Nightscout (typically T1D community). Nightscout ingests Dexcom Share, Abbott Libre, Medtronic, and many other CGM data sources. | 🆕 new |
| Renpho Smart Scale | Smart Scale / Body Composition | M5 (unofficial); M1 fallback (manual CSV) | account credentials (unofficial API) | L | 🟠 Low — no official API, unofficial endpoints fragile, no Rust library. Weight/BMI already in Apple Health via Renpho app sync. | 🆕 new |
| Samsung Health | Platform Health Hub (Android) | M1 (CSV export from Samsung Health app) | none (user-initiated export) | M | 🟡 Medium — no native macOS/Rust API path. M1 CSV export is reliable and self-service. Relevant for Android users or users who switched from Android. | 🆕 new |
| Health Auto Export (iOS app → watch folder) | Apple Health Auto-Export Bridge | M2 (watch folder via iCloud Drive) | iCloud Drive read (no FDA needed since it's iCloud, not a protected system path) | M | 🟢 High — this is the cleanest path for near-real-time Apple Health data without the user manually triggering exports. iOS Shortcuts automation can be set to run daily. Files appear in iCloud Drive on Mac within minutes. | 📋 planned |

### Detail

#### Apple Health export.zip (deep parse) — _Platform Health Hub_

🟢 **High — already built; deepen by adding auto-export watch-folder path and parsing remaining data types not yet extracted** · M1; M2 for auto-export watch folder · none (user-initiated) / iCloud Drive read for M2 · effort **S** · ✅ built

- **Access:** iPhone Health app → Profile photo → Export All Health Data → share export.zip to Mac/iCloud. Alternatively automated via Health Auto Export app → iCloud Drive automation. File path after manual drop: user-chosen. Auto: ~/Library/Mobile Documents/iCloud~com~healthyapps~healthautoexport/Documents/
- **Recommendation:** Build now — extend parsing to cover: workout-routes GPX subfolder, ECG CSVs subfolder, export_cda.xml clinical records, and less-common record types (mindfulness, noise-exposure, mental-health scored assessments, medication dosages, state-of-mind). Also add M2 watch-folder path for Health Auto Export iCloud automation.
- **Notes:** export.xml can exceed 1 GB unzipped for long-term users; streaming XML parse (quick-xml Rust crate) is essential. Key data types in the XML: HKQuantityType (steps, HR, HRV, SpO2, sleep, calories, blood-glucose, blood-pressure, body-mass, stand-hours, noise-exposure, VO2max, respiratory-rate, body-temp, ECG classification, walking/running speed, stair ascents, mindfulness minutes, active energy, basal energy, flights-climbed, swim-stroke-count, cycling cadence, push count, UV-exposure, environmental audio, dietary fields, body-fat %, lean-mass, BMI, waist-circ, handwashing, tooth-brushing, sexual-activity), HKCategoryType (sleep stages, menstrual, cervical-mucus, ovulation-test, pregnancy, lactation, mood, headache, spotting, contraceptives, skin-care, bleed flow), HKWorkout (all workouts with duration/distance/energy + optional route GPX), ECG CSV, HKClinicalRecord. The HealthKitV2 export format (used by apps like MyDataHelps) adds structured JSON for scored assessments. QS Access app is no longer maintained; Health Auto Export (App Store, HealthyApps) is the best third-party auto-export option for the watch-folder path.

#### Oura Ring — _Smart Ring_

🟢 **High — official public API, self-service OAuth app registration at cloud.ouraring.com/oauth/applications** · M5 · OAuth · effort **S** · ✅ built

- **Access:** OAuth 2.0 via cloud.ouraring.com/oauth/authorize; token endpoint cloud.ouraring.com/oauth/token; data at api.ouraring.com/v2/usercollection/{collection}. Collections: daily_sleep, daily_activity, daily_readiness, daily_spo2, daily_stress, daily_cardiovascular_age, daily_resilience, heartrate (5-min intervals), sleep (per-session detail), workout, ring_configuration, personal_info, tag, enhanced_tag, rest_mode_period, sleep_time, vo2_max.
- **Recommendation:** Build now — already built; note Personal Access Tokens were deprecated December 2025, OAuth 2.0 only now. Ensure token refresh is handled.
- **Notes:** Oura does NOT push everything to Apple Health — its 5-minute-interval HRV time-series, detailed sleep stage data with exact timing, readiness/resilience/cardiovascular-age composite scores, and daytime activity sessions are Oura-only fields. Rate limit is not published but generous for personal use. The built collector should backfill from account creation date. Oura syncs basic sleep and activity summaries to Apple Health but strips vendor-specific scores.

#### WHOOP — _Fitness/Recovery Band_

🟢 **High — well-documented public API, v2 active in 2026 (v1 webhooks removed). Self-service developer registration with active WHOOP subscription required.** · M5; M1 fallback (CSV export) · OAuth · effort **M** · 🆕 new

- **Access:** OAuth 2.0; app registration at developer.whoop.com (free, requires active WHOOP membership + device). Endpoints: GET /v1/activity/cycles (physiological cycles with strain/HR), /v1/recovery (recovery score, HRV, RHR), /v1/sleep (sleep performance, stages, quality), /v1/activity/workout (workouts), /v1/user/measurement/body (body measurements), /v1/user/profile/basic. Webhook support available. Export: app → Profile → Privacy → Download My Data → email ZIP with workouts.csv, sleeps.csv, journal_entries.csv (note: recovery/steps/VO2Max NOT in export as of early 2026).
- **Recommendation:** Build now — WHOOP has a popular user base; strain/recovery composite scores are vendor-exclusive and NOT in Apple Health.
- **Notes:** WHOOP does sync basic sleep and activity to Apple Health but strain, recovery score, day-strain breakdown, and sleep performance percentage are WHOOP-exclusive. API requires WHOOP membership (~$30/mo or annual) — this is a hardware/subscription dependency, not a software one. Rate limits not publicly documented. The CSV export is a useful M1 fallback for initial historical import before OAuth backfill. App-only required during registration, but once credentials are issued the pull is standalone.

#### Withings (Scales, Sleep Mat, Blood Pressure, Trackers, ScanWatch) — _Smart Scale / Multi-Device_

🟢 **High — well-documented, self-service, no commercial approval needed for personal/app use. Withings is unique in offering scale-based body composition + ECG + blood pressure + sleep mat + thermometer all under one API.** · M5 · OAuth · effort **M** · 🆕 new

- **Access:** OAuth 2.0 via developer.withings.com (free self-service Public API tier). Endpoints: wbsapi.withings.net/measure (weight, body comp, BP, ECG, temp), /sleep (sleep summary, stages, HRV, respiratory), /activity (steps, distance, calories, hr), /heart (ECG signal + AFib classification), /user. Refresh token valid 1 year.
- **Recommendation:** Build now — the Withings API is broad and high-value: weight/body-composition trends, blood pressure history, and sleep-mat respiratory data are all fields NOT fully represented in Apple Health exports.
- **Notes:** Withings syncs weight, BP, sleep, steps to Apple Health but ECG signals, pulse-wave-velocity, vascular age, and BeamO stethoscope data are API-only. Body Scan (segmental body comp, nerve health EDA) data is accessible via the API but some newer metrics may require the premium API tier. Intelligence/Scores API (vitality score, disease risk) is listed as 'coming soon'. The sleep mat captures respiratory rate, snoring, and sleep disruption without requiring a worn device — unique positioning.

#### Garmin Connect (Garmin watches/GPS devices) — _GPS Watch / Sports Watch_

🟡 **Medium — official API is business-use only and approval-gated; not reliably self-service for a personal app. Full data export (M1) is excellent for historical data and completely self-service. Unofficial API (used by GarminDB, garmin-connect-export scripts) works by reusing the browser session but is fragile and TOS-gray.** · M5; M1 strong fallback · OAuth (official API) / account credentials (export) · effort **L** · 🆕 new

- **Access:** Two paths: (A) Official API: apply at garmin.com/en-US/forms/GarminConnectDeveloperAccess/ — business-use only, approval within 2 days, free access with some health metrics requiring commercial license fee. (B) Full data export: garmin.com/account/datamanagement/ → Export Data → ZIP with FIT files for every activity + health summary CSVs. FIT files parseable with fitparser Rust crate (crates.io). CLI tools like garmin-connect-export can also pull activities via the web interface session.
- **Recommendation:** Build now (M1 export path first; spike official API approval feasibility). Garmin has a very large user base and deep fitness analytics. FIT files contain GPS traces, power meter data, training effect, aerobic/anaerobic TE, HRV at rest, body battery — none of which flow to Apple Health.
- **Notes:** Garmin syncs basic activity summaries, steps, heart rate, sleep, and SpO2 to Apple Health but body battery, training effect, HRV status, VO2Max training readiness, floor climbs, stress score, and full GPS/power FIT data are Garmin-only. The fitparser Rust crate (crates.io) can decode FIT binary files — this enables a pure-Rust collector. GarminDB Python library is a useful reference. Official API program FAQ as of June 2026 says they are accepting applications; an earlier report of suspension appears outdated.

#### Fitbit / Google Health API — _Fitness Tracker_

🟡 **Medium — legitimate API but Google Cloud setup overhead, CASA review for production at scale (not an issue for personal app up to 100 users), and the migration away from Fitbit API must complete by September 2026. Self-service for personal/small scale.** · M5 · OAuth (Google Cloud project required) · effort **M** · 🆕 new

- **Access:** Google Health API (replaces Fitbit Web API September 2026). Setup: Google Cloud project → enable Google Health API → OAuth 2.0. Endpoints cover activity bundles (steps, calories, floors, distance, active-zone-minutes), sleep (stages, durations), heart rate (intraday), HRV, SpO2, respiratory rate, resting HR, body temp, blood glucose. Legacy Fitbit API OAuth (FOT) no longer accepts new integrations; must use Google OAuth 2.0. CASA security review required for >100 users.
- **Recommendation:** Build now — Fitbit has tens of millions of users; many early wearable adopters have years of Fitbit history not in Apple Health. Build against Google Health API directly (not legacy Fitbit API which shuts down September 2026).
- **Notes:** Fitbit syncs basic steps/sleep/HR to Apple Health but intraday HR, HRV, Fitbit-specific scores (Active Zone Minutes), and VO2Max training metrics are API-only. The mandatory Google Cloud project requirement adds friction vs. simpler OAuth apps. No fees for <100 users. Note: Google Fit REST API (separate from Google Health API) is also deprecated and shutting down in late 2026 — do not build against it.

#### Dexcom CGM (G6, G7, ONE, ONE+) — _Continuous Glucose Monitor_

🟢 **High for personal use — developer.dexcom.com is self-service up to 5 authorized users (more than enough for personal app). G6, G7, ONE, ONE+ supported. Note: Dexcom API v2 endpoints shut down May 2026; v3 is the current target.** · M5 · OAuth · effort **M** · 🆕 new

- **Access:** developer.dexcom.com — OAuth 2.0 registration. Endpoints (v3): GET /v3/users/self/egvs (estimated glucose values), /v3/users/self/events (carbs, insulin, exercise, health), /v3/users/self/calibrations, /v3/users/self/alerts, /v3/users/self/devices, /v3/users/self/dataRange. Sandbox available for development. Limited access: up to 5 users (sufficient for personal). Full commercial access requires application.
- **Recommendation:** Build now — CGM data (continuous blood glucose every 5 minutes) is high-value for users managing diabetes or metabolic health, and is not available through Apple Health in its full form.
- **Notes:** Dexcom syncs current glucose to Apple Health as Blood Glucose samples, but the full 5-min EGV stream, event log (carb/insulin entries), calibration history, and alert history are API-only. Stelo (Dexcom's OTC CGM) does NOT have the same API access as G6/G7 as of 2026 — Stelo users may be limited to the app. HIPAA authorization flow is required for production data access. pydexcom is a useful Python reference for the auth flow.

#### Polar (watches and HR monitors) — _Sports Watch / HR Monitor_

🟢 **High — public API, self-service registration (just a Polar Flow account needed), no commercial approval required. Well-documented AccessLink v4.** · M5; M1 fallback · OAuth · effort **M** · 🆕 new

- **Access:** OAuth 2.0 via Polar AccessLink API v4 at www.polar.com/polar-api-v4/. Free developer registration at polar.com/developers/. Data: sleep, HRV, 24/7 HR, training sessions (FIT/TCX/CSV export), daily activity, VO2Max, nightly recharge, fitness test results. Bulk export: support.polar.com → 'download all your data' → ZIP with JSON per session + bulk JSON. Individual activity export: Polar Flow web → session → Export (FIT/TCX/GPX/CSV).
- **Recommendation:** Build now — Polar has a large user base among serious endurance athletes. Nightly recharge (ANS recovery), training load, and orthostatic test data are not in Apple Health.
- **Notes:** Polar syncs basic activity, HR, sleep, and weight to Apple Health but Training Load Pro, Polar's Nightly Recharge ANS score, Running Performance, Cardio Load, and muscle load are API-only. Bulk JSON export does NOT include derived algorithm data (activity/sleep summaries), which is API-only — important caveat to note. The Polar SDK also supports BLE connection to devices for live data but that requires the app running, violating Trove's standalone constraint.

#### Abbott FreeStyle Libre / LibreView (CGM) — _Continuous Glucose Monitor_

🟡 **Medium — no official direct API; unofficial LibreView API is fragile and TOS-gray. CSV export is reliable but manual. The Apple Health sync via LibreLinkUp covers basic glucose values so Apple Health import already captures this for Libre users.** · M1 (CSV export); M5 via aggregator; M5 unofficial fallback · account credentials (unofficial API) / none (CSV export) · effort **L** · 🆕 new

- **Access:** No official public developer API from Abbott. Pathways: (A) LibreView unofficial API (documented at libreview-unofficial.stoplight.io) — reverse-engineered but used by several open-source projects. (B) Third-party aggregators (Terra, Thryve, Junction) that have formal Abbott partnerships. (C) LibreLinkUp companion app pushes Libre readings to Apple Health. (D) LibreView web → Patients → Export data → CSV (glucose readings with timestamps).
- **Recommendation:** Spike first — implement M1 CSV export parser first (straightforward), then evaluate the unofficial LibreView API stability. The LibreLinkUp → Apple Health path means many Libre users are already covered by the existing Apple Health import.
- **Notes:** LibreView CSV export contains glucose readings at scan frequency (every 15 min for Libre 2/3). The unofficial LibreView API (libreview-unofficial.stoplight.io) uses HTTPS POST to libreview-us.abbott.com with an app-id header and account credentials; it retrieves the same data as the LibreView patient portal. Abbott has formal partnerships with platforms like Tidepool (FDA-cleared data flow) for clinical integrations. Libre 3 data syncs to Apple Health via LibreLinkUp app.

#### Strava (workout activities with GPS) — _Workout/Activity Platform_

🟢 **High — public self-service OAuth, extremely well-documented, enormous user base. Strava often aggregates workouts from Garmin, Apple Watch, Suunto, Polar, Wahoo — so it can be the single pull for multi-device athletes.** · M5 · OAuth · effort **M** · 🆕 new

- **Access:** OAuth 2.0 at developers.strava.com. Self-service app registration. API v3 endpoints: /athlete/activities (list with pace/HR/power summaries), /activities/{id} (full detail), /activities/{id}/streams (raw time-series: HR, cadence, power, altitude, GPS, velocity), /activities/{id}/laps. Rate limits: 100 requests/15 min + 1000/day per athlete.
- **Recommendation:** Build now — Strava is the de facto social layer for GPS workouts; many users have their entire workout history here including data from devices they no longer own. GPS stream data, segment efforts, and power data are not in Apple Health.
- **Notes:** Strava does NOT provide sleep, passive health metrics, or recovery data — it is workout/activity only. Strava can be a fallback or supplement for Garmin/Polar users whose direct API access is blocked. Activity GPS streams (lat/lng/alt time-series) require a separate API call per activity. Webhook subscriptions allow real-time new-activity notification. Must honor Strava's API Agreement which prohibits bulk resale or public display without attribution.

#### Withings Smart Scale (standalone — Withings users without other Withings devices) — _Smart Scale / Body Composition_

🟢 **High — same API as main Withings entry; separate entry here because many users own only a Withings scale without other Withings devices.** · M5 · OAuth · effort **S** · 🆕 new

- **Access:** Same Withings OAuth API as above (developer.withings.com). wbsapi.withings.net/measure?action=getmeas returns weight, fat mass%, muscle mass, bone mass, water%, visceral fat index, basal metabolic rate, pulse wave velocity, vascular age.
- **Recommendation:** Build now — covered by the main Withings collector; no separate implementation needed.
- **Notes:** Withings syncs weight and BMI to Apple Health but segmental body composition, visceral fat, vascular age, and BMR are API-only. The Withings Body Scan ($400+) scale adds 6-lead ECG and nerve health EDA — these are also API-accessible.

#### Omron Blood Pressure Monitors — _Blood Pressure / Cardiac_

🟡 **Medium — OMRON Connect Create requires contacting them for developer onboarding (not pure self-service). libomron (github.com/openyou/libomron) provides direct device access but coverage varies by model and the project is aging. OMRON syncs BP to Apple Health via the official app.** · M5; M3-adjacent (libomron direct device read) · OAuth; none for libomron (USB access) · effort **M** · 🆕 new

- **Access:** OMRON Connect Create API at digitalhealth.omronconnect.com. OAuth 2.0. Data API provides BP readings (systolic/diastolic/pulse), irregular pulse flag. Device SDK for direct BLE integration. Also: libomron open-source library on GitHub can read data directly from Omron BT devices over USB/HID on macOS.
- **Recommendation:** Spike first — many Apple Health users already have Omron BP data in their export.zip (if they use the OMRON Connect app). Evaluate whether Apple Health import already covers the use case sufficiently before building a direct integration.
- **Notes:** If the user has the OMRON Connect iOS app, all BP readings are already in Apple Health export. The direct Omron API adds value primarily for users who don't sync to Apple Health. OMRON Connect Create availability varies by region. libomron supports older serial/BT models only.

#### Coros (GPS sports watches) — _GPS Watch / Sports Watch_

🟡 **Medium — application-gated, not self-service. Unclear if approved for individual developers vs. business entities only. The COROS MCP server (May 2026) is an interesting M6-adjacent path but requires the user to be running it.** · M5 · OAuth (requires application approval) · effort **L** · 🆕 new

- **Access:** COROS API requires formal application at support.coros.com/hc/en-us/articles/17085887816340-Submitting-an-API-Application. Business/developer partnership model — not pure self-service. Provides: activities, training plans, daily health, sleep data. Also: COROS now offers an MCP server (announced May 2026) allowing AI access to COROS Training Hub data.
- **Recommendation:** Spike first — submit an API application and document the outcome. COROS has a growing user base among runners/triathletes. FIT file export from COROS app is a reliable M1 fallback (COROS exports .fit files for individual activities).
- **Notes:** COROS syncs basic activity and health data to Apple Health. The COROS MCP server (May 2026) announced at the5krunner.com operates as an AI data interface but would require the user to keep the MCP server running — violating Trove's standalone constraint for regular sync. FIT export files (parseable via fitparser crate) are the most reliable path.

#### Suunto (GPS watches) — _GPS Watch / Sports Watch_

🟡 **Medium — public documentation exists but 'companies and organizations' framing suggests business-use bias; personal developer access unclear. The SuuntoPlus opening (March 2026) is for watch-face/app developers, not necessarily the Cloud API. Suunto syncs to Apple Health.** · M5 · OAuth (application approval required) · effort **L** · 🆕 new

- **Access:** Suunto Cloud API at apizone.suunto.com (Azure API Management). OAuth 2.0. Provides: workout FIT files, route data, 24/7 activity data, sleep data via webhooks or polling. Suunto opened SuuntoPlus to all developers March 2026 with no prior relationship required. However, the Cloud API for data extraction still has a longer approval process for commercial integrations.
- **Recommendation:** Build later — smaller user base vs. Garmin/Polar; Apple Health captures most data. Consider FIT file export as M1 fallback.
- **Notes:** Suunto exports individual activities as FIT/GPX from Suunto app (one at a time) and provides a bulk JSON export from suunto.com. Webhook notifications for new workouts (FIT file URLs) are the cleanest integration path if API access is granted. Suunto syncs activity, HR, and sleep to Apple Health but training load and swim-specific metrics are API-only.

#### Eight Sleep Pod (smart mattress) — _Sleep Tech / Smart Mattress_

🟡 **Medium — unofficial cloud API works but is TOS-gray and fragile at API changes. Free Sleep local path is technically excellent (local SQLite, full biometric fidelity) but requires user to root their Pod, which is a significant barrier. Eight Sleep syncs some sleep data to Apple Health but biometric detail (HR, HRV, breath rate time series, room temperature) is not in Apple Health.** · M5 (unofficial cloud API); M3-adjacent (local Free Sleep SQLite) · account credentials (unofficial cloud); SSH to device (Free Sleep) · effort **L** · 🆕 new

- **Access:** No official public API. Unofficial paths: (A) Reverse-engineered cloud API (Home Assistant integration at github.com/lukas-clarke/eight_sleep) using OAuth2 against app.eightsleep.com — active as of 2026. (B) Free Sleep project (github.com/throwaway31265/free-sleep) — open-source local server that roots Pod 3 and exposes REST API + SQLite at /persistent/free-sleep-data/free-sleep.db with vitals at /api/metrics/vitals (HR, HRV, breath rate, biometrics every 2 sec).
- **Recommendation:** Spike first — the unofficial cloud API (lukas-clarke's Home Assistant integration) is the pragmatic path; implement it as an M5 collector with clear user disclosure that it's unofficial. Document Free Sleep local path for power users.
- **Notes:** Eight Sleep sleep data (total sleep, score, stages) does sync to Apple Health but the full biometric stream (beat-by-beat HR, HRV, breath rate, toss/turn events, room temperature adjustments) is only available through the API or Free Sleep local path. Free Sleep (December 2025) demonstrates complete local data extraction with no cloud dependency and is reversible. Pod 3 is the confirmed supported device.

#### Amazfit / Zepp Health (Xiaomi smartwatches) — _Smartwatch_

🟠 **Low — no official API, unofficial endpoints frequently change, no Rust library. Apple Health sync via Zepp app covers most data points for Apple Health users.** · M1 (GPX export per workout); M5 unofficial cloud · account credentials (unofficial) · effort **XL** · 🆕 new

- **Access:** No official public API. Paths: (A) Amazfit app syncs to Apple Health (steps, HR, sleep, SpO2) — covered by Apple Health import. (B) Zepp OS developer platform (developer.zepp.com) for building watch apps, not for data extraction. (C) Reverse-engineered cloud API: api-mifit.huami.com (or regional variants) with apptoken header, endpoint /v1/sport/run/history.json. Active community projects exist for extraction. (D) Export from Zepp app: one-at-a-time GPX export for workouts.
- **Recommendation:** Icebox — Apple Health import already covers Amazfit users on iPhone. The unofficial API is fragile. Build only if user demand is significant.
- **Notes:** Amazfit syncs steps, HR, sleep, SpO2 to Apple Health on iOS. Workout GPS GPX exports are the only reliable non-API path. The Zepp Health HAID integration project (haid.app) documents the unofficial API but is a community effort without stability guarantees.

#### Ultrahuman Ring AIR — _Smart Ring_

🟡 **Medium — gated developer program, not self-service. Growing ring segment; Ultrahuman is unique in combining ring biometrics with optional CGM in one API. Data does sync to Apple Health partially.** · M5 · OAuth (application approval required) · effort **L** · 🆕 new

- **Access:** UltraSignal developer platform at vision.ultrahuman.com/developer-docs. OAuth 2.0. REST API. Requires applying to developer program (not instant self-service) — priority given to compelling app proposals. Developer kit loan available upon approval. Data: Recovery Score, Sleep Score, Movement Index, HRV, resting heart rate, skin temperature deviation, nightly SpO2, optionally CGM (M1 patch) glucose data.
- **Recommendation:** Build later — smaller user base than Oura. Apply for developer access and note the outcome. Priority after WHOOP and Withings.
- **Notes:** Ultrahuman syncs basic sleep and activity to Apple Health. The API's unique value is the optional M1 CGM patch integration — real-time glucose + biometrics in a single request — which has no Apple Health equivalent. Access tokens valid for 1 week with refresh. The developer kit loan reduces hardware cost for integration development.

#### Nightscout (self-hosted CGM aggregator) — _Continuous Glucose Monitor / Open Platform_

🟢 **High — clean REST API, fully open-source, no approval needed. Relevant only for users who self-host Nightscout (typically T1D community). Nightscout ingests Dexcom Share, Abbott Libre, Medtronic, and many other CGM data sources.** · M5 (pull from user's own Nightscout instance) · API secret / JWT (user provides their own Nightscout URL + token) · effort **S** · 🆕 new

- **Access:** Nightscout is user-self-hosted (Node.js app on a cloud host or local machine). REST API: GET <nightscout-url>/api/v3/entries (glucose readings), /api/v3/treatments (insulin, carbs), /api/v3/devicestatus. Authenticated with an API secret or JWT token. nightscout.github.io — actively maintained, v2 API added 'easy state' statistics in 2026.
- **Recommendation:** Build later — niche but technically trivial. Users who run Nightscout are technically sophisticated and would value this. Build after Dexcom direct API.
- **Notes:** Nightscout aggregates data from multiple CGM brands including Dexcom, Abbott, Medtronic, Eversense. If a user runs Nightscout it becomes a single endpoint for ALL their CGM history regardless of device brand. The Nightscout Connect plugin (2026) can also import from vendor clouds. Trove pull = GET /api/v3/entries?count=10000&token=<token> with pagination.

#### Renpho Smart Scale — _Smart Scale / Body Composition_

🟠 **Low — no official API, unofficial endpoints fragile, no Rust library. Weight/BMI already in Apple Health via Renpho app sync.** · M5 (unofficial); M1 fallback (manual CSV) · account credentials (unofficial API) · effort **L** · 🆕 new

- **Access:** No official public API. Unofficial API reverse-engineered: POST https://renpho.qnclouds.com/api/v3/users/sign_in.json (with app_id), then GET user measurements. Python renpho-api package on PyPI. Also: Terra API has a Renpho integration (third-party aggregator). CSV export possible from Renpho iOS app (manual, limited). Renpho syncs weight to Apple Health via app.
- **Recommendation:** Icebox — Apple Health import covers weight data for Renpho users on iPhone. Renpho's body composition metrics (fat%, muscle mass) do NOT reliably sync to Apple Health, but the unofficial API instability makes this a low-priority build.
- **Notes:** Renpho body composition (fat%, muscle%, bone, water) does not push to Apple Health — only weight does. For users wanting body-comp trends from Renpho, the unofficial API or manual CSV export are the only paths. Renpho-api PyPI package (neilzilla/hass-renpho lineage) demonstrates the auth flow. This would need a Rust re-implementation or subprocess call.

#### Samsung Health — _Platform Health Hub (Android)_

🟡 **Medium — no native macOS/Rust API path. M1 CSV export is reliable and self-service. Relevant for Android users or users who switched from Android.** · M1 (CSV export from Samsung Health app) · none (user-initiated export) · effort **M** · 🆕 new

- **Access:** Samsung Health Data SDK (developer.samsung.com/health/data) — Android-only SDK. Health Connect (Android) provides an alternative cross-vendor path on Android. No macOS SDK. Data export: Samsung Health app → More → Settings → Download personal data → ZIP with CSV files for all categories.
- **Recommendation:** Build later — Samsung Health CSV export parser is an M1 import path similar to Apple Health export.zip. Useful for Android-primary users or Samsung Galaxy Watch owners who don't use Apple Health.
- **Notes:** Samsung Health CSV export includes: steps, heart rate, floors, sleep (with stages), workouts, blood oxygen, blood pressure, blood glucose, stress level, body composition (via Samsung scales), ECG, temperature. Galaxy Watch data lands here. Since Trove is macOS-first, Samsung Health is primarily a historical-import use case (user was on Android, wants to import history). Samsung Health also syncs to Google Fit/Health Connect.

#### Health Auto Export (iOS app → watch folder) — _Apple Health Auto-Export Bridge_

🟢 **High — this is the cleanest path for near-real-time Apple Health data without the user manually triggering exports. iOS Shortcuts automation can be set to run daily. Files appear in iCloud Drive on Mac within minutes.** · M2 (watch folder via iCloud Drive) · iCloud Drive read (no FDA needed since it's iCloud, not a protected system path) · effort **M** · 📋 planned

- **Access:** App Store: Health Auto Export - JSON+CSV (id1115567069, HealthyApps). Automation: iCloud Drive sync writes to ~/Library/Mobile Documents/iCloud~com~healthautoexport~healthautoexport/ on Mac (accessible without FDA once iCloud Drive is mounted). Exports 150+ HealthKit metric types as CSV or JSON on iOS Shortcuts schedule or background refresh. API Export feature documented at github.com/Lybron/health-auto-export.
- **Recommendation:** Build now — this enables the M2 watch-folder path that makes Apple Health collection continuous rather than one-shot. The iCloud Drive path is accessible on Mac without Full Disk Access.
- **Notes:** Health Auto Export updated for iOS 26 (June 2026) with medication dosage export and iOS 26 design. Each automation produces a JSON or CSV file in a named subfolder. The API Export feature (documented on GitHub) allows posting directly to a REST endpoint — useful if Trove ever runs a local HTTP collector. The free tier limits metrics; paid tier ($4.99/yr) unlocks all 150+. QS Access (the legacy alternative) is no longer maintained.

### Health: Wearables & Biometrics — cross-cutting notes

1. APPLE HEALTH AS AGGREGATION HUB: For iPhone users, the Apple Health export.zip (already built) captures data from nearly every wearable sold in the US — Garmin, Fitbit, Polar, Withings, Coros, Suunto, Amazfit, Omron, Dexcom, Libre — in basic form. Direct vendor API integrations are justified primarily for vendor-exclusive fields (recovery scores, training analytics, HRV detail, ECG signals) and for users who don't use an iPhone. This should inform build prioritization: Apple Health depth first, then APIs for the vendor-exclusive fields. 2. FIT FILE PARSING: Garmin, Polar, Coros, and Suunto all export or sync .fit binary files. The fitparser Rust crate (crates.io) decodes all of them. Building a shared FIT file ingestor in trove-core enables M1 import for all these sources without separate API integrations. 3. BUSINESS-USE API GATING: Garmin Connect API (official), Suunto Cloud API, and Coros API are formally business-use or require applications. For a public app with compiled-in credentials this is navigable, but the application/approval process adds lead time. FIT file export is the universal M1 fallback for all three. 4. OAUTH SELF-SERVICE TIER: Oura (already built), WHOOP, Withings, Polar, Dexcom, Fitbit/Google Health, and Strava are all self-service OAuth — the cleanest M5 path. These should be prioritized for direct API builds. 5. UNOFFICIAL APIS: Eight Sleep, Renpho, and Amazfit/Zepp have no public APIs; community reverse-engineering exists but is TOS-gray and fragile. Apple Health sync covers most user cases for Amazfit; Eight Sleep's unofficial cloud API is the only practical path for detailed sleep biometrics. 6. COMPILATION STRATEGY: The fitparser Rust crate plus Apple Health XML parsing (quick-xml) covers the M1/M2 surface for most wearables. For OAuth pulls, a shared hyper/reqwest-based OAuth2 token manager already exists in trove-core (from Oura and TickTick). All new wearable API integrations should reuse this infrastructure. 7. CGM SPECIFICITY: Both Dexcom (official API) and Libre (informal via Apple Health or LibreView unofficial API) are worth building. These are niche but high-value; the Dexcom API is the only one with FDA-cleared real-time data pipeline. Nightscout is a single endpoint for multi-brand CGM aggregation for technically sophisticated users. 8. BODY COMPOSITION BLIND SPOT: Weight syncs to Apple Health from all scales, but body composition details (fat%, muscle mass, visceral fat, segmental body comp) typically do NOT — Withings API and Samsung Health CSV export are the two reliable paths to capture this data.

---

## Health: Nutrition, Medical Records, Labs & Genetics

This domain spans several distinct data categories: nutrition tracking (primarily M1 export from premium apps), medical records (FHIR-based patient portals via SMART on FHIR OAuth, very strong ecosystem in 2026), lab results (Quest/Labcorp both support FHIR patient access), genetics (file import from 23andMe/AncestryDNA — stable exports despite 23andMe bankruptcy), and CGM glucose monitoring (Dexcom has a real OAuth API; Abbott LibreView is OAuth but partner-only). Apple Health's export.zip already brings in nutrition fields written by any HealthKit-connected app, so it is the passive aggregation layer for nutrition on iPhone — but it is iOS-only and Mac has no live HealthKit API. The overall domain is highly feasible: nearly every source has either a CSV/export path (M1) or an established OAuth/FHIR path (M5), and almost nothing requires a permanently running external process. The biggest gap is dental records, which lack patient-accessible FHIR in practice despite the standard existing.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Apple Health Export (Nutrition fields) | Nutrition | M1 | None (user-initiated export on device). macOS side: parse XML in Trove. | S | 🟢 High — export.zip is already built in Trove for Health data; nutrition fields are just additional XML record types to parse. No new mechanism needed. | ✅ built |
| MyFitnessPal | Nutrition | M1 | None for export. OAuth/API: not available to new developers. | S | 🟢 High — CSV export is clean and well-documented. Requires Premium subscription ($10/mo). API path is blocked for new integrations. | 🆕 new |
| Cronometer | Nutrition | M1 | None for export (web login). No public API; unofficial session scraping is user-only. | S | 🟢 High — export is free, documented, produces clean CSVs with full micronutrient detail (100+ nutrients). Cronometer is widely regarded as having the most complete micronutrient database. | 🆕 new |
| MacroFactor | Nutrition | M1 | None for export. | S | 🟢 High — clean CSV export, no paywall friction. The Firestore-based unofficial API is a bonus path that could enable M5-like automation but is unsupported. | 🆕 new |
| Epic MyChart / SMART on FHIR Patient Portals | Medical Records | M5 | OAuth2 via MyChart credentials (user-driven). No macOS TCC permission needed. | L | 🟢 High — 42% of US hospitals were certified as of mid-2024, growing. USCDI v3 is the federal floor from Jan 2026. Patient-facing standalone launch is the primary path. Epic sandbox available for development at no cost. | 🆕 new |
| CMS Blue Button 2.0 (Medicare Claims) | Medical Records | M5 | OAuth2 via Medicare.gov. No TCC needed. | M | 🟢 High — well-established public API, no cost, sandbox environment, active development. Only relevant for users with Medicare (65+ or disability). | 🆕 new |
| Quest Diagnostics (MyQuest / FHIR) | Labs | M5; M1 fallback | OAuth2 via MyQuest credentials. No TCC. | M | 🟢 High — Quest is the largest US lab network. FHIR endpoint is live; LOINC-coded results enable structured storage. PDF download is the M1 fallback (less structured). | 🆕 new |
| Labcorp Patient FHIR | Labs | M5; M1 fallback | OAuth2 via Labcorp patient account. | M | 🟢 High — Labcorp confirmed Apple Health Records integration; FHIR patient registration portal exists. Same approach as Quest. | 🆕 new |
| Apple Health Records (Clinical FHIR via iPhone) | Medical Records | M1 | iOS HealthKit access (iPhone only). On macOS Trove cannot read HealthKit live. The M1 path: user exports individual CDA documents from iPhone Health app, or connects providers and then the SMART on FHIR path above is the better route. | M | 🟡 Medium — the data is rich and standardized, but the primary access is iPhone-only HealthKit. No Mac API. M1 workaround: user can share individual clinical documents as CDA/PDF files from iPhone to Mac. Better path is direct FHIR pull (Epic/Quest/etc.) which bypasses Apple entirely. | 🆕 new |
| 23andMe Raw Genetic Data | Genetics | M1 | None (user downloads from web). | S | 🟢 High — export still works as of June 2026 despite bankruptcy/acquisition by TTAM Research Institute. File format is stable and well-documented. | 🆕 new |
| AncestryDNA Raw Genetic Data | Genetics | M1 | None. | S | 🟢 High — well-documented stable export, Ancestry company is healthy (not in bankruptcy). Only test owner or account manager can download. | 🆕 new |
| Dexcom CGM (Continuous Glucose Monitor) | Labs / Metabolic | M1; M5 | OAuth2 via Dexcom account. No TCC. | M | 🟡 Medium — CSV export (M1) is freely available. API (M5) requires Dexcom partnership approval, which may be difficult for an independent app. Recommend M1 CSV import as primary, document M5 API as aspirational. | 🆕 new |
| Abbott FreeStyle Libre (LibreView) CGM | Labs / Metabolic | M1; M5 (partner-only) | OAuth2 via LibreView account for API. No TCC. | S | 🟡 Medium — CSV export (M1) works freely. M5 API is blocked for independent apps without Abbott partnership. | 🆕 new |
| Lab PDF Import (Quest, Labcorp, any lab) | Labs | M1 | None. | M | 🟢 High — PDFs are universally available from all labs and portals. Parsing accuracy depends on extractor quality; modern LLM-assisted parsing is quite good for structured lab reports. | 🆕 new |
| Cronometer (CSV export detailed) | Nutrition | M1 | None. | S | 🟢 High | 🆕 new |
| Bearable (Symptom / Mood / Medication Tracker) | Symptoms & Mood | M1 | None. | S | 🟢 High — consistent CSV format documented enough for third-party parsing (GitHub: samstarling/bearable-csv). 900K+ users. Covers the chronic illness / medication tracking use case well. | 🆕 new |
| Generic FHIR Patient Access (multi-EHR) | Medical Records | M5 | OAuth2 per provider institution. No TCC. | L | 🟢 High — standardized across vendors. Single SMART on FHIR client implementation works against all compliant systems. Server discovery is the main UX challenge (user must input or select their health system). | 🆕 new |
| Noom | Nutrition | M1 | None. | S | 🟠 Low — 30-day wait, GDPR-request style delivery, no documented format, no API, data completeness unknown. | 🆕 new |
| Lifesum | Nutrition | M1 | None for web export. | S | 🟡 Medium — 7-day web export is friction-free; full history requires manual request. Apple Health passthrough makes M1 via export.zip a reasonable proxy. | 🆕 new |
| Medisafe (Medication Tracker) | Medications | M1 | None. | S | 🟡 Medium — data is valuable (medication adherence) but paywalled with no free export tier as of 2026. | 🆕 new |
| Lose It! | Nutrition | M1 (indirect via Apple Health) | None for Apple Health path. | S | 🟠 Low — no direct export path for personal use. Apple Health passthrough is the only practical route. | 🆕 new |
| Withings (Smart Scale / Health Monitor) | Body Composition | M5; M1 fallback | OAuth2 via Withings account. | M | 🟢 High — well-documented public API, OAuth2, no partnership barrier. Withings is popular for smart scales and body composition. Body composition data (fat%, muscle, bone mass) not available from other sources. | 🆕 new |
| Dental Records | Dental | M1 | None for PDF drop. | S | 🟠 Low — FHIR standard exists but adoption is nascent. Practical access is PDF only. PDF parsing for dental records is less structured than medical lab PDFs. | 🆕 new |
| 23andMe / AncestryDNA — Variant Analysis | Genetics | M1 (raw file already imported; analysis is local processing) | None — local processing only. | M | 🟢 High — raw file import (M1) is straightforward. Parsing to extract rsIDs and cross-reference against ClinVar/SNPedia is a pure local computation. No external API needed for basic variant cataloging. | 🆕 new |
| Pharmacy Prescriptions (CVS, Walgreens, etc.) | Medications | M1; M5 (via Blue Button for Medicare) | OAuth2 for Blue Button. None for PDF drop. | S | 🟡 Medium — no direct pharmacy API for consumers. FHIR MedicationRequest (from EHR pull) and Blue Button Part D (Medicare) provide structured prescription data. Direct pharmacy portal PDFs are M1. | 🆕 new |
| Levels Health (CGM + Metabolic App) | Labs / Metabolic | M1 | None. | S | 🟡 Medium — export works but Levels is a subscription service and the data is largely a processed/annotated view of the underlying CGM data (already capturable directly from Dexcom/Abbott). The food log with glucose response correlation is the unique Levels-specific data. | 🆕 new |

### Detail

#### Apple Health Export (Nutrition fields) — _Nutrition_

🟢 **High — export.zip is already built in Trove for Health data; nutrition fields are just additional XML record types to parse. No new mechanism needed.** · M1 · None (user-initiated export on device). macOS side: parse XML in Trove. · effort **S** · ✅ built

- **Access:** iPhone: Health app → Profile → Export All Health Data → export.zip. Fields: HKQuantityTypeIdentifierDietaryEnergyConsumed, DietaryProtein, DietaryCarbohydrates, DietaryFatTotal, DietaryFiber, DietaryWater, DietarySodium, plus ~30 micro/vitamin fields. Written by any HealthKit-connected calorie app (MacroFactor, Cronometer, MyFitnessPal, etc.).
- **Recommendation:** Build now — extend existing export.zip parser to also extract nutrition HKQuantityType records. Low incremental cost.
- **Notes:** Health Records (clinical FHIR) are NOT included in the export.zip XML; they require separate extraction. Nutrition data is only present if the user has logged food via a HealthKit-connected app. Apple's XML schema is undocumented and has changed across iOS versions.

#### MyFitnessPal — _Nutrition_

🟢 **High — CSV export is clean and well-documented. Requires Premium subscription ($10/mo). API path is blocked for new integrations.** · M1 · None for export. OAuth/API: not available to new developers. · effort **S** · 🆕 new

- **Access:** Export: Settings → Account → Download Your Data (Premium/Premium+ only) — ZIP with 3 CSVs: Meal Level Nutrition Details (macros/micros per meal + timestamps), Progress History (weight/measurements), Exercise History. Delivered via email link within 1 hour. Public API: closed/invite-only as of 2026; not accepting new applications.
- **Recommendation:** Build now — parse the Premium export ZIP. Macro/micro data per meal is high value. The API gate is a non-issue since M1 export is clean enough for retrospective capture.
- **Notes:** Premium paywall means free users cannot export at all. Export is one-shot retrospective, not continuous. Micro-nutrient coverage is the best of any nutrition app export. No M5 path available.

#### Cronometer — _Nutrition_

🟢 **High — export is free, documented, produces clean CSVs with full micronutrient detail (100+ nutrients). Cronometer is widely regarded as having the most complete micronutrient database.** · M1 · None for export (web login). No public API; unofficial session scraping is user-only. · effort **S** · 🆕 new

- **Access:** Export: cronometer.com → Account → Export Data → choose type (Diary/Servings/Biometrics/Exercises/Notes) → CSV. No paid tier required for basic export; Gold tier adds timestamps. Unofficial scraping library: gocronometer (Go) uses the same session-based export API the web app uses — works per-user only.
- **Recommendation:** Build now — Cronometer is the gold standard for micronutrient completeness; parsing its export CSV is straightforward. No API access needed for M1 import.
- **Notes:** No official public API for individual users. The unofficial gocronometer library could theoretically drive a semi-automated M2/M6 path but is session-scraped, fragile, and against ToS for non-personal use. Stick with M1. Gold tier needed for per-entry timestamps (useful for meal timing analysis).

#### MacroFactor — _Nutrition_

🟢 **High — clean CSV export, no paywall friction. The Firestore-based unofficial API is a bonus path that could enable M5-like automation but is unsupported.** · M1 · None for export. · effort **S** · 🆕 new

- **Access:** Export: app → Settings → Export Your Data → Granular Export (per data type) or Quick Export (summary). CSVs for weight trend, expenditure, calories/macros, targets. Unofficial Rust crate macro-factor-api (lib.rs/crates/macro-factor-api) reads from Firebase/Firestore REST API using standard Firebase auth — works with active subscription.
- **Recommendation:** Build now — MacroFactor is popular among fitness-focused users; export parsing is trivial. The unofficial Firestore API is interesting for a future M5 spike but not necessary.
- **Notes:** Requires active MacroFactor subscription ($11.99/mo or $69.99/yr). Export focuses on energy/macro tracking more than micronutrients. No official API.

#### Epic MyChart / SMART on FHIR Patient Portals — _Medical Records_

🟢 **High — 42% of US hospitals were certified as of mid-2024, growing. USCDI v3 is the federal floor from Jan 2026. Patient-facing standalone launch is the primary path. Epic sandbox available for development at no cost.** · M5 · OAuth2 via MyChart credentials (user-driven). No macOS TCC permission needed. · effort **L** · 🆕 new

- **Access:** SMART on FHIR standalone launch: app registers at open.epic.com (free, 750+ APIs, USCDI v3 coverage). Patient authenticates with MyChart credentials via OAuth2, authorizes scopes (patient/*.read), app receives FHIR R4 resources: Conditions, Medications, Immunizations, Observations (labs), AllergyIntolerance, DiagnosticReport, Procedures, CarePlan, DocumentReference. Production access: submit app via open.epic.com, no cost, reviewed in days. Also works against Cerner/Oracle Health, Meditech, Allscripts (all USCDI-mandated as of Jan 2026).
- **Recommendation:** Build now — this is the canonical path for structured clinical records in the US. Epic alone covers a majority of US patients. Implement as a SMART on FHIR standalone client: register app on open.epic.com, use Authorization Code + PKCE, pull FHIR R4 bundles, write to vault as FHIR NDJSON or structured markdown. Effort L because FHIR parsing is rich but well-spec'd.
- **Notes:** User must know their provider uses Epic (or another FHIR-compliant EHR). Auth is per-organization: each hospital system is a separate authorization endpoint. Discovery via FHIR .well-known/smart-configuration. Health Records data does NOT appear in Apple Health export.zip, so this FHIR pull is the only structured path. USCDI v5 support from Epic is in development for future expanded data elements.

#### CMS Blue Button 2.0 (Medicare Claims) — _Medical Records_

🟢 **High — well-established public API, no cost, sandbox environment, active development. Only relevant for users with Medicare (65+ or disability).** · M5 · OAuth2 via Medicare.gov. No TCC needed. · effort **M** · 🆕 new

- **Access:** https://bluebutton.cms.gov/ — OAuth2 app registration (free, reviewed). Beneficiary authorizes via Medicare.gov credentials. FHIR R4 + CARIN IG: ExplanationOfBenefit, Coverage, Patient. Returns Part A (inpatient), Part B (outpatient/physician), Part D (prescriptions) claims data for 64M+ Medicare enrollees.
- **Recommendation:** Build now — high-value claims-level data (what was billed, prescribed, diagnosed) not accessible any other way. Relatively narrow audience (Medicare) but very high leverage for that group. Implement as M5 OAuth pull alongside Epic FHIR for full medical picture.
- **Notes:** Claims data (billing codes, dates, providers, drug NDCs) not clinical notes. Complements Epic FHIR records nicely. EOB resources contain diagnosis codes (ICD-10), procedure codes (CPT/HCPCS), drug NDCs. Users must be Medicare beneficiaries. Sandbox at sandbox.bluebutton.cms.gov for development.

#### Quest Diagnostics (MyQuest / FHIR) — _Labs_

🟢 **High — Quest is the largest US lab network. FHIR endpoint is live; LOINC-coded results enable structured storage. PDF download is the M1 fallback (less structured).** · M5; M1 fallback · OAuth2 via MyQuest credentials. No TCC. · effort **M** · 🆕 new

- **Access:** MyQuest portal (myquest.questdiagnostics.com): patient login, view/download PDF results per test. FHIR patient access: Quest exposes LabResults as FHIR Observation resources (LOINC codes, values, units, reference ranges, flags) via DiagnosticReport. Patient FHIR endpoint: api.questdiagnostics.com (FHIR). Also: SMART on FHIR integration — test results flow into Apple Health if Quest is connected as a Health Records provider.
- **Recommendation:** Build now — structured lab results (LOINC codes, values, reference ranges) are high-value for longitudinal health tracking. Start with FHIR pull via open.epic.com (Quest results appear there if Epic is connected) or Quest's own FHIR endpoint.
- **Notes:** FHIR 3rd-party identity verification may be required (MyQuest updated their FHIR policy to require identity verification for third-party app access). PDF fallback always works — add to generic lab PDF import feature. Quest results often already appear in Epic FHIR bundles if ordered through a connected health system.

#### Labcorp Patient FHIR — _Labs_

🟢 **High — Labcorp confirmed Apple Health Records integration; FHIR patient registration portal exists. Same approach as Quest.** · M5; M1 fallback · OAuth2 via Labcorp patient account. · effort **M** · 🆕 new

- **Access:** fhir.labcorp.com/register/patient/ — patient FHIR API registration. OAuth2; patients can connect third-party apps. Returns lab results as FHIR Observation/DiagnosticReport resources. Also integrates with Apple Health Records — Labcorp results pull to iPhone via Health Records provider link. PDF download from patient.labcorp.com portal is M1 fallback.
- **Recommendation:** Build now alongside Quest — implement a generic FHIR lab results importer (works against any FHIR R4 lab endpoint) rather than two separate integrations.
- **Notes:** Labcorp's FHIR endpoint details are less publicly documented than Quest's, but patient registration portal exists. Results often also appear in Epic FHIR if ordered through an Epic-connected system.

#### Apple Health Records (Clinical FHIR via iPhone) — _Medical Records_

🟡 **Medium — the data is rich and standardized, but the primary access is iPhone-only HealthKit. No Mac API. M1 workaround: user can share individual clinical documents as CDA/PDF files from iPhone to Mac. Better path is direct FHIR pull (Epic/Quest/etc.) which bypasses Apple entirely.** · M1 · iOS HealthKit access (iPhone only). On macOS Trove cannot read HealthKit live. The M1 path: user exports individual CDA documents from iPhone Health app, or connects providers and then the SMART on FHIR path above is the better route. · effort **M** · 🆕 new

- **Access:** iPhone only: Health app → Browse → Health Records → Add Account → search provider → authenticate with patient portal credentials → FHIR R4 data pulls automatically to on-device encrypted store. Data: conditions, labs, medications, immunizations, procedures, vitals, allergies. Export from iPhone via share sheet per-record or via export.zip (NOTE: clinical records are NOT included in export.zip XML — they exist only in HealthKit's clinical record store, accessible only via HealthKit API on iOS).
- **Recommendation:** Spike first — direct FHIR pulls (Epic, Quest, Labcorp) are strictly superior and Mac-native. Apple Health Records as a separate M1 import is a secondary path for users who have already linked providers on iPhone but don't want to re-auth with Trove. Consider accepting individual CDA XML file drops.
- **Notes:** Clinical records in HealthKit are HKClinicalRecord objects with a fhirResource property. They are NOT in export.zip. Only accessible programmatically via HealthKit API on iOS/iPadOS (not Mac). A future iOS companion app could extract and vault them, but that is out of scope for the Mac-first phase.

#### 23andMe Raw Genetic Data — _Genetics_

🟢 **High — export still works as of June 2026 despite bankruptcy/acquisition by TTAM Research Institute. File format is stable and well-documented.** · M1 · None (user downloads from web). · effort **S** · 🆕 new

- **Access:** 23andme.com → Settings → Privacy & Data → Manage My Data → Download Raw Data → enter password + consent checkbox → email link (~15 min). File: genome_*.txt.zip — tab-delimited 4-column file (rsid, chromosome, position, genotype) covering ~650,000 SNPs for v5 chip. Can convert to VCF with bcftools or open-source scripts.
- **Recommendation:** Build now — M1 file import. Store as vault/genetics/23andme_raw.txt.gz. Parse to extract key SNPs or store raw for AI analysis. Very high personal value for users who have tested.
- **Notes:** 23andMe was acquired in bankruptcy by TTAM Research Institute (Anne Wojcicki's new entity). Download still works June 2026 but could change without warning — users should download promptly. Privacy concern: raw genome is the most sensitive personal data; vault-local storage is a strong value proposition. VCF conversion enables downstream bioinformatics. No live API; export-only.

#### AncestryDNA Raw Genetic Data — _Genetics_

🟢 **High — well-documented stable export, Ancestry company is healthy (not in bankruptcy). Only test owner or account manager can download.** · M1 · None. · effort **S** · 🆕 new

- **Access:** ancestry.com → DNA → Settings → Actions → scroll to 'Download Raw DNA Data' → password confirmation + consent → email link (~15 min). File: AncestryDNA.txt (zip) — same 4-column tab-delimited format as 23andMe but different chip coverage (~700,000 SNPs, AffymetrixGenomeWide Human SNP Array).
- **Recommendation:** Build now alongside 23andMe — same parser handles both formats (same 4-column TSV structure, minor header differences). Combined, these two cover the vast majority of consumer genetic testing.
- **Notes:** Ancestry focuses on ethnicity estimates and family trees; raw SNP file is the same concept as 23andMe. File is larger than 23andMe (~150MB unzipped). No API. FamilyTreeDNA also offers a similar export for users who tested there — same format, could be included.

#### Dexcom CGM (Continuous Glucose Monitor) — _Labs / Metabolic_

🟡 **Medium — CSV export (M1) is freely available. API (M5) requires Dexcom partnership approval, which may be difficult for an independent app. Recommend M1 CSV import as primary, document M5 API as aspirational.** · M1; M5 · OAuth2 via Dexcom account. No TCC. · effort **M** · 🆕 new

- **Access:** Two paths: (1) Dexcom Clarity web (clarity.dexcom.com) — log in, click Export icon, choose date range → CSV of all EGVs (estimated glucose values) + events. (2) Dexcom Developer API (developer.dexcom.com) — OAuth2, approved developer program, REST API v3: /egvs, /events, /calibrations, /alerts, /devices, /dataRange endpoints. OpenAPI 3.0.3 spec available. Requires applying to Dexcom Strategic Partnerships team for production access.
- **Recommendation:** Build now for M1 CSV import. Spike the M5 OAuth API path — if Dexcom approves partnership, M5 enables continuous sync. CGM data is extremely high-value for metabolic health tracking.
- **Notes:** CGM data: 288 readings/day (every 5 min), 30+ days per export. Paired with food logs and activity data this is transformative for nutrition-metabolic correlation. Abbott FreeStyle Libre (LibreView) is the other major CGM — their API is partner-only OAuth (similar situation). LibreView CSV export also available from clarity.libreview.com. Both should be in M1 import support.

#### Abbott FreeStyle Libre (LibreView) CGM — _Labs / Metabolic_

🟡 **Medium — CSV export (M1) works freely. M5 API is blocked for independent apps without Abbott partnership.** · M1; M5 (partner-only) · OAuth2 via LibreView account for API. No TCC. · effort **S** · 🆕 new

- **Access:** LibreView web (libreview.com) — log in, navigate to Reports, export CSV of glucose readings. LibreView API: OAuth2 via Abbott ecosystem — partner-only, requires Abbott partnership agreement. Third-party platform integration (Thryve, Validic) provides normalized API but adds a middleman.
- **Recommendation:** Build now for M1 CSV import (reuse Dexcom CSV parser, same basic structure). Document M5 as future-when-partnership-available.
- **Notes:** FreeStyle Libre 3 data flows from sensor → LibreLink app → LibreView cloud. CSV export covers all historical readings. Market share is roughly even with Dexcom in some demographics. Important to support both.

#### Lab PDF Import (Quest, Labcorp, any lab) — _Labs_

🟢 **High — PDFs are universally available from all labs and portals. Parsing accuracy depends on extractor quality; modern LLM-assisted parsing is quite good for structured lab reports.** · M1 · None. · effort **M** · 🆕 new

- **Access:** User downloads PDF from patient portal (MyQuest, patient.labcorp.com, hospital portals) and drops into Trove. Trove uses PDF text extraction + pattern matching / LLM to parse: test name, result value, unit, reference range, date, ordering provider, patient demographics.
- **Recommendation:** Build now — this is the universal fallback for any lab or hospital the user connects to, even those without FHIR. Works for international users, dental labs, specialty labs. Complements the FHIR pull nicely.
- **Notes:** Quest PDFs include LabCorp logo, demographics, test name, result, unit, reference range, flags. Layout is consistent per lab. Open-source pdfium or pdf-extract crate can handle text extraction; then a rules/LLM layer normalizes. Store both raw PDF and extracted structured JSONL. LOINC code mapping can be approximate for non-FHIR sources.

#### Cronometer (CSV export detailed) — _Nutrition_

🟢 **High** · M1 · None. · effort **S** · 🆕 new

- **Access:** Already covered above. Emphasis: Daily Nutrition export contains 100+ nutrient columns per day including full amino acid profile, all vitamins (A, B1-B12, C, D, E, K), minerals, omega-3/6 fatty acids, individual sugars — unmatched micronutrient breadth among consumer apps.
- **Recommendation:** Build now
- **Notes:** Duplicate entry for emphasis on unique value. Free tier export available. Gold adds per-entry timestamps essential for meal-timing analysis.

#### Bearable (Symptom / Mood / Medication Tracker) — _Symptoms & Mood_

🟢 **High — consistent CSV format documented enough for third-party parsing (GitHub: samstarling/bearable-csv). 900K+ users. Covers the chronic illness / medication tracking use case well.** · M1 · None. · effort **S** · 🆕 new

- **Access:** Bearable app → Settings → Export Data → CSV. Exports: mood ratings, pain scores, fatigue, symptoms, medications taken, lifestyle factors (sleep hours, steps, etc.), custom factors — all per-entry with timestamps. Free tier supports basic export; premium ($34.99/yr) unlocks full history export.
- **Recommendation:** Build now — fills the manual symptom/medication tracking niche that no other structured source covers. Particularly valuable for users with chronic conditions. CSV format is parseable.
- **Notes:** No public API. Scheduled auto-export is on Bearable's roadmap but not shipped. Manual export is the only current path. Complements medication prescription data from pharmacy/EHR by adding adherence and symptom-response tracking.

#### Generic FHIR Patient Access (multi-EHR) — _Medical Records_

🟢 **High — standardized across vendors. Single SMART on FHIR client implementation works against all compliant systems. Server discovery is the main UX challenge (user must input or select their health system).** · M5 · OAuth2 per provider institution. No TCC. · effort **L** · 🆕 new

- **Access:** Any FHIR R4 + SMART on FHIR compliant EHR: Epic (open.epic.com), Cerner/Oracle Health, Meditech Expanse, Allscripts, athenahealth, and ~40+ others. Patient standalone launch: app presents FHIR server URL (or discovers via FHIR .well-known/smart-configuration), redirects to provider login, receives access token, pulls patient resources. USCDI v3 mandated from Jan 2026.
- **Recommendation:** Build now — this should be built as a single generic SMART on FHIR client, not per-EHR integrations. Register on open.epic.com for Epic coverage. Implement server-URL input + FHIR capability discovery for any compliant EHR. Resources to target: Patient, Condition, MedicationRequest, Immunization, Observation (labs+vitals), AllergyIntolerance, DiagnosticReport, Procedure, DocumentReference.
- **Notes:** Building on the Trove FHIR client covers Epic, Cerner, Quest, Labcorp, and dozens more with one implementation. The main Rust FHIR libraries: fhir-rs (basic), or hand-roll against the FHIR R4 spec using reqwest + serde_json. The complexity is schema breadth, not auth.

#### Noom — _Nutrition_

🟠 **Low — 30-day wait, GDPR-request style delivery, no documented format, no API, data completeness unknown.** · M1 · None. · effort **S** · 🆕 new

- **Access:** GDPR/CCPA data request only: Settings → Account → Manage Subscription → Request My Data, or email gdprsupport@noom.com. Data delivered within 30 days. No direct CSV export, no API. Noom licenses food database from MyNetDiary.
- **Recommendation:** Icebox — data access is friction-heavy and Noom's meal logging isn't as micronutrient-rich as Cronometer/MFP. Only worthwhile if users specifically request it.
- **Notes:** Noom pivoted toward behavioral coaching; its nutrition data is less complete than dedicated trackers. Users wanting structured exports are better served by switching to Cronometer/MFP.

#### Lifesum — _Nutrition_

🟡 **Medium — 7-day web export is friction-free; full history requires manual request. Apple Health passthrough makes M1 via export.zip a reasonable proxy.** · M1 · None for web export. · effort **S** · 🆕 new

- **Access:** lifesum.com/account/export-data — quick 7-day export directly. Full history: contact support for GDPR data dump. Writes nutrition data to Apple Health (iOS) so the Health export.zip path captures Lifesum data indirectly. No public API.
- **Recommendation:** Build later — Lifesum users are largely covered by the Apple Health nutrition export (Lifesum writes to HealthKit). Direct Lifesum export adds incremental value only for users who want the original per-food-entry data vs aggregated Apple Health totals.
- **Notes:** Lifesum syncs to Apple Health on iOS — so Trove's existing Apple Health export already captures Lifesum totals. Direct export worth adding if users request it.

#### Medisafe (Medication Tracker) — _Medications_

🟡 **Medium — data is valuable (medication adherence) but paywalled with no free export tier as of 2026.** · M1 · None. · effort **S** · 🆕 new

- **Access:** Premium only (subscription required since Jan 2026 paywall): in-app Reports → Export → choose medication/timeframe → email CSV. Free users have no export access. CSV covers adherence rates, doses taken/missed, notes.
- **Recommendation:** Build later — medication adherence data from Medisafe is useful but paywall limits user base. Bearable covers medication tracking for many users. Better approach: ingest medications from FHIR MedicationRequest resources (EHR-sourced) and pair with Bearable symptom data.
- **Notes:** Medisafe moved to mandatory paid subscription in January 2026. Free users can no longer export. Consider supporting as M1 for Premium users. Alternative: many users are migrating away from Medisafe post-paywall.

#### Lose It! — _Nutrition_

🟠 **Low — no direct export path for personal use. Apple Health passthrough is the only practical route.** · M1 (indirect via Apple Health) · None for Apple Health path. · effort **S** · 🆕 new

- **Access:** Validic connector exists for enterprise B2B. No public API for individual users. No documented self-serve CSV export in 2026 (unlike Cronometer/MFP). Food data writes to Apple Health (iOS). Export via Validic is enterprise/clinical only.
- **Recommendation:** Icebox for direct integration. Apple Health nutrition export already captures Lose It! data for iOS users. Skip dedicated Lose It! integration unless users request it.
- **Notes:** Lose It! uses a crowdsourced food database. Users who care about data portability tend to prefer Cronometer or MFP. Market share declining.

#### Withings (Smart Scale / Health Monitor) — _Body Composition_

🟢 **High — well-documented public API, OAuth2, no partnership barrier. Withings is popular for smart scales and body composition. Body composition data (fat%, muscle, bone mass) not available from other sources.** · M5; M1 fallback · OAuth2 via Withings account. · effort **M** · 🆕 new

- **Access:** API: developer.withings.com — OAuth2 public API. Endpoints: /measure/getmeas (weight, fat%, bone mass, muscle mass, hydration, visceral fat, BMI), /sleep/get (sleep stages), /activity/getactivity (steps, calories). Also: Withings Health dashboard → Settings → Download my data → CSV (weight data). Webhook/notification system for real-time updates.
- **Recommendation:** Build now — Withings API is clean, public, and provides body composition data that complements fitness trackers. Register on developer.withings.com. Pairs well with Oura (already built) for holistic body metrics.
- **Notes:** Withings devices: Body/Body+/Body Cardio scales, ScanWatch, sleep mat. API returns body composition per weigh-in which is uniquely valuable. Different from Oura (already built in Trove).

#### Dental Records — _Dental_

🟠 **Low — FHIR standard exists but adoption is nascent. Practical access is PDF only. PDF parsing for dental records is less structured than medical lab PDFs.** · M1 · None for PDF drop. · effort **S** · 🆕 new

- **Access:** HL7 Dental Data Exchange IG (v2.0-ballot 2025) defines FHIR profiles — but vendor adoption is voluntary and limited in 2026. Dentrix Enterprise has some EHR integrations; Open Dental has a REST API. In practice: patient PDFs (X-rays, treatment notes, perio charts) downloaded from dental practice portal or requested as records. No standardized patient-facing FHIR export widely available.
- **Recommendation:** Build later — accept PDF drop from dental portals as part of a generic medical document import. Structured dental data is not extractable at scale in 2026. Include in generic document vault rather than a dedicated dental connector.
- **Notes:** Dental records are highly fragmented across thousands of private practices. No major equivalent of Epic for dental. Open Dental (open-source PMS) has a REST API but this requires the user's dentist to run Open Dental, which is rare. Future: as HL7 Dental IG adoption grows, revisit.

#### 23andMe / AncestryDNA — Variant Analysis — _Genetics_

🟢 **High — raw file import (M1) is straightforward. Parsing to extract rsIDs and cross-reference against ClinVar/SNPedia is a pure local computation. No external API needed for basic variant cataloging.** · M1 (raw file already imported; analysis is local processing) · None — local processing only. · effort **M** · 🆕 new

- **Access:** After importing raw .txt file (see 23andMe/AncestryDNA entries above): SNPedia database (snpedia.com) provides manually curated SNP-phenotype associations. bcftools can convert to VCF. Open-source analysis: github.com/heiner/snpedia-23andme. Promethease (MyHeritage, ~$12 one-time) provides detailed health report. USDA dbSNP / ClinVar are public reference databases.
- **Recommendation:** Build later — raw file import is M1 and high priority. Local SNP analysis (rsID → ClinVar annotation) is a compelling follow-on feature: bundle a snapshot of ClinVar's VCF or SNPedia's dump for offline analysis. This is a strong differentiator for privacy (no data leaves the machine).
- **Notes:** ClinVar provides free bulk downloads (FTP at ftp.ncbi.nlm.nih.gov/pub/clinvar). SNPedia data is available for bulk download to registered users. Bundling a trimmed ClinVar snapshot into the Trove binary or shipping it as a user-installable data pack would enable fully local variant annotation — a strong privacy story compared to Promethease/MyHeritage.

#### Pharmacy Prescriptions (CVS, Walgreens, etc.) — _Medications_

🟡 **Medium — no direct pharmacy API for consumers. FHIR MedicationRequest (from EHR pull) and Blue Button Part D (Medicare) provide structured prescription data. Direct pharmacy portal PDFs are M1.** · M1; M5 (via Blue Button for Medicare) · OAuth2 for Blue Button. None for PDF drop. · effort **S** · 🆕 new

- **Access:** Walgreens: developer.walgreens.com has a Prescription Refill API (B2B, not personal data pull). CVS: no developer API for personal prescription data. Practical paths: (1) FHIR MedicationRequest resources from Epic/Cerner (prescriptions written by providers appear in FHIR pull). (2) PDF prescription history from CVS/Walgreens patient portal (print/download). (3) Medicare Part D claims via Blue Button 2.0 (most complete for Medicare users — drug NDC, fill dates, days supply, prescribers).
- **Recommendation:** Build now (indirectly) — the Epic FHIR pull (MedicationRequest resources) and Blue Button 2.0 (Part D drug claims) together cover prescriptions for most users without needing a dedicated pharmacy integration. Add PDF import from pharmacy portals as a secondary path.
- **Notes:** Walgreens prescription API is B2B (refill ordering), not personal data retrieval. CVS has no consumer-facing API. The FHIR + Blue Button combo is the right path. For non-Medicare users without Epic, pharmacy portal PDF is the only option.

#### Levels Health (CGM + Metabolic App) — _Labs / Metabolic_

🟡 **Medium — export works but Levels is a subscription service and the data is largely a processed/annotated view of the underlying CGM data (already capturable directly from Dexcom/Abbott). The food log with glucose response correlation is the unique Levels-specific data.** · M1 · None. · effort **S** · 🆕 new

- **Access:** Levels app: support.levels.com/article/105-export → CSV exports for Glucose/CGM data, Activity Logs, Food Logs (with nutritional metadata), Zones (glucose response scores). No public API documented. Levels sources CGM data from Dexcom or Abbott via those devices' APIs — so Dexcom CSV export is a more direct path.
- **Recommendation:** Build later — the glucose data is better sourced from Dexcom/Abbott directly. The unique Levels value is glucose-food correlation scores (Zones), which are worth ingesting if users have Levels. Low priority.
- **Notes:** Levels is a premium subscription ($200+/yr). Market is growing but niche. The glucose-metabolic score data is Levels-proprietary and useful for AI correlation analysis but not available without an active subscription.

### Health: Nutrition, Medical Records, Labs & Genetics — cross-cutting notes

1. FHIR as a unifying mechanism: A single SMART on FHIR R4 client implementation in Trove covers Epic, Cerner, Oracle Health, Meditech, Quest, Labcorp, and any other USCDI v3-compliant EHR (federally mandated from Jan 2026). This one build covers medical records, lab results, medications, immunizations, and vitals across the majority of the US healthcare system. Build this first before any EHR-specific integrations. Register on open.epic.com (free, covers 750+ APIs, no usage cost).

2. Apple Health export.zip already in Trove covers nutrition fields: The existing export.zip parser just needs to handle additional HKQuantityType records (DietaryEnergyConsumed, DietaryProtein, etc.). Any HealthKit-connected nutrition app (MFP, MacroFactor, Cronometer iOS, Lose It, Lifesum) writes to HealthKit, so extending the existing parser is the highest-leverage nutrition move for iPhone users.

3. Clinical Records are NOT in export.zip: Apple Health clinical records (from connected FHIR providers) live only in HealthKit's clinical record store, which is iOS-only and not accessible from macOS. The direct FHIR pull (Epic/Quest/etc.) is the correct Mac-native path — it bypasses Apple entirely.

4. Genetics: Both 23andMe and AncestryDNA use the same 4-column TSV format. A single parser covers both. The ClinVar bulk download (public domain, FTP) enables fully local variant annotation — a strong privacy differentiator vs. online tools like Promethease/MyHeritage.

5. CGM data (Dexcom/Abbott) pairs powerfully with nutrition logs: Timestamp-aligned glucose + food entries enable metabolic correlation analysis that is uniquely valuable. Both have M1 CSV exports as the pragmatic starting point; Dexcom also has an OAuth API (partnership required). This should be treated as a priority cluster alongside nutrition.

6. Sensitive data handling: Genetic raw files (650K+ SNPs) and clinical FHIR records (diagnoses, medications) are the most sensitive data types in the vault. Trove's local-first, files-as-truth model is a genuine competitive advantage here. Consider per-collection encryption at rest as an opt-in for the genetics and medical records folders.

7. Blocked/low paths: Dental records lack any practical structured patient access in 2026 (FHIR IG exists but adoption is near-zero). Noom and Lose It! have no usable direct export; Apple Health passthrough is the only practical path. Pharmacy APIs (CVS, Walgreens) are B2B order APIs, not personal data retrieval — use FHIR MedicationRequest + Blue Button instead.

---

## Computer & Developer Activity

This domain covers what happens on the machine itself — the full trail of developer and computer activity beyond the already-built activity watcher and Screen Time. It spans four tiers: (1) local plain-file sources requiring zero permissions (shell history, git logs, Claude Code transcripts, download history, screenshots folder), (2) other apps' local SQLite/state databases requiring Full Disk Access (VS Code globalStorage, Cursor chat, Copilot sessions, Qbserve, Alfred clipboard), (3) cloud API pulls for coding platform data (GitHub, GitLab, WakaTime), and (4) third-party time-tracking apps (Timing, Qbserve) that store data locally and offer programmatic export. Feasibility is uniformly High for local-file sources given Trove already has the M3 copy-then-read pattern and FDA grant; cloud API pulls are straightforward M5 OAuth; the main blockers are a handful of encrypted or proprietary stores (Raycast, Windsurf remote-stored threads) and the standalone constraint eliminating screen-recording-based keystroke loggers.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Shell History (~/.zsh_history) | Terminal & Shell | M3 | none | S | 🟢 High — plain text, no permissions, already noted as 'trivially easy, surprisingly rich' in data-sources.md. Extended format gives per-command timestamps. The existing zsh_history-analysis OSS tooling confirms the format is stable. | 📋 planned |
| Local Git Activity (all repos) | Version Control | M3 | none | S | 🟢 High — gitoxide/gix is pure Rust, production-quality (used by Cargo), no permissions needed, reads .git directly. The gitoxide-core-tools-query feature even includes an auto-maintained SQLite DB for analytics. Already listed as planned in data-sources.md. | 📋 planned |
| Claude Code Session History | AI Session Transcripts | M3 | none | S | 🟢 High — plain JSONL files in home dir, no permissions needed, format is well-documented (session file format article by Yi Huang on Medium, simonw/claude-code-transcripts OSS tooling confirms stability). Already listed as planned in data-sources.md. | 📋 planned |
| GitHub (commits, PRs, issues, stars, gists) | Developer Platform — Cloud | M5 | OAuth / API key (PAT) | S | 🟢 High — well-documented REST API, PAT-based (no OAuth server needed for personal use), 5,000 req/hr with PAT, all personal data available. Already listed as planned in data-sources.md. | 📋 planned |
| Cursor IDE Chat History | AI Session Transcripts | M3 | none (home-dir files, no FDA needed) | M | 🟢 High — local SQLite files, no special permissions needed (home dir). The vibe-replay.com deep-dive confirms exact schema. Medium effort because the data is spread across multiple DBs and requires joining meta + blobs tables; the global state.vscdb key prefix scheme needs decoding. | 🆕 new |
| VS Code Recent Workspaces & File Activity | IDE Activity | M3 | none | S | 🟢 High — plain SQLite in home dir, well-documented key schema, no permissions needed. Gives 'what projects/files did I open in VS Code' without needing WakaTime. | 🆕 new |
| WakaTime Coding Stats | Coding Time Tracking — Cloud | M5 | API key | S | 🟢 High — well-documented REST API, personal API key available in account settings, no OAuth server needed, rich data (file, project, language, editor, OS per heartbeat). The offline BoltDB is Go-specific and not easily readable from Rust (skip it; the API covers everything). | 🆕 new |
| GitLab Activity (commits, MRs, issues) | Developer Platform — Cloud | M5 | OAuth / API key (PAT) | S | 🟢 High — well-documented REST API, PAT-based, works for both gitlab.com and self-hosted instances. Same pattern as GitHub connector. | 🆕 new |
| Alfred Clipboard History | Clipboard Manager | M3 | none (home-dir path, no FDA needed) | S | 🟢 High — plain SQLite, home dir, no special permissions, well-documented schema (confirmed by multiple independent sources including gist.github.com/pirate/6551e1c00a7c4b0c607762930e22804c). Alfred requires the Powerpack (~$34 one-time) for clipboard history, so not universally applicable. | 🆕 new |
| Qbserve App Time Tracker | Productivity Tracking — Local | M3 | none | M | 🟡 Medium — the DB path is confirmed and SQLite is readable, but the schema is undocumented and would require reverse-engineering. Not universally applicable (paid app, niche). The app tracks URLs via browser extensions (Firefox/Vivaldi/Opera/Yandex — notably not Chrome or Safari natively). | 🆕 new |
| Timing App (macOS Time Tracker) | Productivity Tracking — Local | M6 | none (AppleScript) or localhost HTTP (Web API, Expert plan) | M | 🟡 Medium — data is locked behind a proprietary store with no direct DB path. Programmatic access requires AppleScript or the paid Web API (not all users have Expert plan). Timing is subscription-based (~$10/month). Not universally applicable. | 🆕 new |
| macOS Download History (QuarantineEventsV2) | File System Activity | M3 | none (in ~/Library/Preferences, accessible without FDA) | S | 🟢 High — home-dir SQLite, no permissions, records persist even after files are moved/deleted. Captures downloads from Safari, Chrome, Firefox, Mail, and any quarantine-aware app. Records the source URL — richer than just the Downloads folder listing. | 🆕 new |
| Screenshots Folder Metadata | File System Activity | M3 | none (Desktop is accessible; custom locations may need FDA if inside protected dirs) | S | 🟢 High — plain files, timestamp in filename, Spotlight tag confirms screenshot vs. other image. No OCR content capture (that is the explicitly iceboxed Rewind-style feature); just metadata (count, time-of-day distribution, file size). | 🆕 new |
| Zed Editor AI Conversation History | AI Session Transcripts | M3 | none | M | 🟡 Medium — paths have changed across Zed versions (JSON → SQLite with compressed blobs), and there is no official documentation of the storage format. The GitHub discussion #32335 confirms threads.db exists but the blob format is undocumented. Zed is increasingly popular but the storage is in flux. | 🆕 new |
| Windsurf (Cascade) Chat History | AI Session Transcripts | M3 | none | M | 🟡 Medium — the path is confirmed by community sources but the exact file format inside the cascade/ directory is undocumented. The app is under active renaming/rebrand (Windsurf → Devin Desktop), which creates path instability risk. | 🆕 new |
| GitHub Copilot Chat Sessions (VS Code) | AI Session Transcripts | M3 | none | S | 🟢 High — local JSON/JSONL files, no permissions, well-documented by the VS Code community. The kafumanto/copilot-tokens tool confirms the schema is legible. VS Code ≥1.109 format is the current standard. | 🆕 new |
| Bitbucket Activity (commits, PRs) | Developer Platform — Cloud | M5 | OAuth / API key (App Password) | S | 🟡 Medium — Bitbucket's API is well-documented but less feature-rich than GitHub's (no user activity events feed); iterating repos to find commits is more expensive. Bitbucket market share has declined significantly; lower priority than GitHub/GitLab. | 🆕 new |
| iTerm2 Command & Directory History | Terminal & Shell | M3 | none | M | 🟡 Medium — the storage path is not publicly documented and requires filesystem inspection. iTerm2 is popular among macOS developers but not universal. The shell history (~/.zsh_history) covers the same commands more accessibly. | 🆕 new |
| macOS Recent Files (SFL2 / SharedFileList) | File System Activity | M3 | none (~/Library/Application Support is accessible) | L | 🟠 Low — while the files are accessible, SFL2 format uses NSKeyedArchiver with opaque Bookmark data (no readable file paths). Parsing requires either a macOS Objective-C/Swift bridge or reverse-engineering the Bookmark binary format. The files are described as containing 'inscrutable UUIDs and chunks of gibberish text' (Eclectic Light Company). High implementation cost for data that largely overlaps with Spotlight's kMDItemLastUsedDate. | 🆕 new |
| ChatGPT Conversation Export | AI Session Transcripts | M1 | none (one-shot import) | S | 🟢 High — official export, well-documented JSON format, stable since 2023. The export is periodic (manual trigger) rather than live, but covers complete history. | 🆕 new |
| FSEvents File System Journal | File System Activity | M3 | Full Disk Access (for /.fseventsd/ read) — requires root or FDA | L | 🟠 Low — requires root access to read /.fseventsd/ directly. While the Rust library exists, the forensic-level detail (every file system event) is extreme noise for a personal vault and the data volume is massive. The 'what files did I change' question is better answered by git activity (developer repos) and the QuarantineEvents DB (downloads). | 🆕 new |
| JetBrains IDE Activity (IntelliJ, WebStorm, PyCharm, etc.) | IDE Activity | M3 | none | M | 🟡 Medium — the local history feature stores file-level edit events but in a proprietary binary format (not SQLite). Without a time-tracking plugin, there is no structured coding-time data. The WakaTime plugin for JetBrains sends data to the WakaTime API (covered above). Effort increases because format reverse-engineering would be needed. | 🆕 new |

### Detail

#### Shell History (~/.zsh_history) — _Terminal & Shell_

🟢 **High — plain text, no permissions, already noted as 'trivially easy, surprisingly rich' in data-sources.md. Extended format gives per-command timestamps. The existing zsh_history-analysis OSS tooling confirms the format is stable.** · M3 · none · effort **S** · 📋 planned

- **Access:** Plain text file at ~/.zsh_history; extended format (HISTFILE with EXTENDED_HISTORY) prepends : <unix_timestamp>:0; per-session files at ~/.zsh_sessions/*.history on macOS Catalina+. ~/.bash_history is the fallback for bash users.
- **Recommendation:** Build now — zero-permission S effort, rich signal for 'what did I do today', direct complement to the activity watcher.
- **Notes:** Parse `EXTENDED_HISTORY` timestamps when present (`: 1700000000:0;command` format); fall back to line ordering when absent. Deduplicate across per-session files and the main history. Write to `developer/shell/YYYY-MM.jsonl` with fields: ts, command, duration_secs (from the 0-field, always 0 in zsh), session. Incremental cursor = last-imported line offset or timestamp. Fish shell (~/.local/share/fish/fish_history) uses YAML format — worth supporting. No sensitive command redaction at collection time (raw vault); analysis layer can flag secrets.

#### Local Git Activity (all repos) — _Version Control_

🟢 **High — gitoxide/gix is pure Rust, production-quality (used by Cargo), no permissions needed, reads .git directly. The gitoxide-core-tools-query feature even includes an auto-maintained SQLite DB for analytics. Already listed as planned in data-sources.md.** · M3 · none · effort **S** · 📋 planned

- **Access:** Walk configured repo root dirs (user-configured list in Trove settings, e.g. ~/Projects, ~/Code). Use gix (gitoxide) crate — already in the Rust ecosystem, used by Cargo itself. Read commit log: author, timestamp, message, repo, branch, files-changed stats via gix-traverse.
- **Recommendation:** Build now — S effort, no permissions, pure local, high-value 'what did I build' signal that pairs naturally with the activity watcher and shell history.
- **Notes:** User configures a list of repo root dirs (or a parent dir to scan). Collect: repo path, branch, commit sha (short), ts, author email, subject, insertions, deletions, files changed. Write to `developer/git/YYYY-MM.jsonl`. Incremental by per-repo highest-imported commit timestamp stored in .trove/git-sync.json. Avoid reading submodule .git dirs twice. The `gix` crate handles packed-refs, shallow clones, and worktrees gracefully. Effort is S because the gix API for walking commits is well-documented.

#### Claude Code Session History — _AI Session Transcripts_

🟢 **High — plain JSONL files in home dir, no permissions needed, format is well-documented (session file format article by Yi Huang on Medium, simonw/claude-code-transcripts OSS tooling confirms stability). Already listed as planned in data-sources.md.** · M3 · none · effort **S** · 📋 planned

- **Access:** JSONL files at ~/.claude/projects/<project-slug>/<session-id>.jsonl. Each line is a typed message record (user prompt, assistant response with content blocks, tool calls, tool results, system prompts, summaries, git snapshots). Global index at ~/.claude/history.jsonl (prompt text, timestamp, project path, session ID). CLAUDE_CONFIG_DIR env var overrides the base path.
- **Recommendation:** Build now — zero friction, S effort, uniquely valuable 'what did I work on with AI' signal; complements git activity and shell history perfectly.
- **Notes:** Import strategy: read history.jsonl for the index, then read individual session files. Fields to extract: session_id, project_path (derive repo/project name), start_ts (first message), end_ts (last message), message_count, tool_calls (summarized by type), summary text (the auto-generated summary if present). Avoid storing full conversation text in vault by default (privacy, size); store metadata + summary only; full text is an opt-in toggle. Cursor = per-session-id seen set stored in .trove/claude-sync.json. The raine/claude-history fuzzy-search tool confirms the format is stable and readable.

#### GitHub (commits, PRs, issues, stars, gists) — _Developer Platform — Cloud_

🟢 **High — well-documented REST API, PAT-based (no OAuth server needed for personal use), 5,000 req/hr with PAT, all personal data available. Already listed as planned in data-sources.md.** · M5 · OAuth / API key (PAT) · effort **S** · 📋 planned

- **Access:** REST API v2026-03-10 at api.github.com. PAT with read:user, repo scopes. Key endpoints: GET /user/events (public activity feed, 300-event rolling window), GET /users/{user}/repos, GET /repos/{owner}/{repo}/commits?author={user}, GET /user/starred, GET /gists. GitHub also offers account data archive export (Settings → Export account data) as a ZIP. Third-party tool ghexport (Python) covers events/repos/stars comprehensively.
- **Recommendation:** Build now — S effort (PAT-only path), high-value complement to local git. Local git misses: PRs, issues, stars, gists, activity on repos you don't have cloned.
- **Notes:** The /user/events feed only returns the last 300 public events — insufficient for backfill. For commits, iterate repos and walk /commits?author=. For a full historical picture, the GitHub account data archive ZIP is the M1 fallback (contains all commits, issues, PRs in JSON). Suggested: M5 incremental pull for ongoing activity + M1 archive import for backfill. Rate limit: 5,000 req/hr per PAT; for most personal users, iterating repos page-by-page on first run is feasible. Write to developer/github/commits/YYYY-MM.jsonl, developer/github/prs/YYYY-MM.jsonl, developer/github/stars.jsonl.

#### Cursor IDE Chat History — _AI Session Transcripts_

🟢 **High — local SQLite files, no special permissions needed (home dir). The vibe-replay.com deep-dive confirms exact schema. Medium effort because the data is spread across multiple DBs and requires joining meta + blobs tables; the global state.vscdb key prefix scheme needs decoding.** · M3 · none (home-dir files, no FDA needed) · effort **M** · 🆕 new

- **Access:** Primary: ~/.cursor/chats/*/*/store.db (SQLite, tables: meta + blobs). Global state: ~/Library/Application Support/Cursor/User/globalStorage/state.vscdb (1+ GB SQLite, cursorDiskKV table with composerData/bubbleId/agentKv prefixed keys). Agent transcripts: ~/.cursor/projects/*/agent-transcripts/*.jsonl. Also workspace-level: ~/Library/Application Support/Cursor/User/workspaceStorage/<hash>/state.vscdb.
- **Recommendation:** Build later — M effort to extract usefully; same value proposition as Claude Code history but harder to parse. Prioritize after Claude Code history (S effort) is done.
- **Notes:** The store.db files are the most tractable: meta.value gives agentId/name/mode/lastUsedModel as JSON; blobs rows contain conversation content. The state.vscdb cursorDiskKV approach requires decoding compressed JSON blobs keyed by prefix. Start with store.db files for a v1 that captures session metadata + model info + rough message count. Full conversation replay requires state.vscdb blobs. Copy-then-read (Cursor locks its DBs while running). Write to developer/cursor/YYYY-MM.jsonl. Note: Cursor is VS Code-based so the workspaceStorage Copilot chatSessions pattern also applies.

#### VS Code Recent Workspaces & File Activity — _IDE Activity_

🟢 **High — plain SQLite in home dir, well-documented key schema, no permissions needed. Gives 'what projects/files did I open in VS Code' without needing WakaTime.** · M3 · none · effort **S** · 🆕 new

- **Access:** ~/Library/Application Support/Code/User/globalStorage/state.vscdb (SQLite, ItemTable). Key: 'history.recentlyOpenedPathsList' → recently opened folders/files with timestamps. Workspace-level: ~/Library/Application Support/Code/User/workspaceStorage/<hash>/state.vscdb (codelens/cache2 key = files opened per workspace with line counts). Backups: ~/Library/Application Support/Code/Backups/ (unsaved edits).
- **Recommendation:** Build later — useful complement to git activity and shell history, but lower priority than the planned sources. Good S-effort add-on once the M3 pattern is well-established in the codebase.
- **Notes:** Also applicable to Cursor (same VS Code foundation, same paths under ~/Library/Application Support/Cursor/). The Copilot chat sessions live at ~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/*.json (VS Code ≥1.109 format: append-only .jsonl mutation log). Copy-then-read (VS Code holds a write lock on state.vscdb while running, though SQLite WAL mode usually allows concurrent reads). Write to developer/vscode/YYYY-MM.jsonl with fields: ts, path, type (file/folder/workspace).

#### WakaTime Coding Stats — _Coding Time Tracking — Cloud_

🟢 **High — well-documented REST API, personal API key available in account settings, no OAuth server needed, rich data (file, project, language, editor, OS per heartbeat). The offline BoltDB is Go-specific and not easily readable from Rust (skip it; the API covers everything).** · M5 · API key · effort **S** · 🆕 new

- **Access:** REST API at api.wakatime.com/api/v1/ with API key (base64 Basic Auth or Bearer). Key endpoints: /users/current/heartbeats (individual events), /users/current/durations (15-min aggregates), /users/current/summaries (daily breakdowns by language/editor/project/OS), /users/current/stats/{range} (all-time stats), /users/current/projects (project list). Local offline cache: ~/.wakatime/offline_heartbeats.bdb (BoltDB key-value store, Go format — not directly readable from Rust without a BoltDB parser).
- **Recommendation:** Build later — high-value for users who already have WakaTime installed, but it requires a pre-existing WakaTime subscription/account. Not universally applicable (unlike git/shell history). Good for a second wave of developer integrations. Self-hosted Wakapi (open source, SQLite backend) is the privacy-forward alternative worth supporting simultaneously.
- **Notes:** WakaTime requires the user to have already installed the WakaTime plugin in their editor — Trove doesn't install it. So this is an import of pre-existing data, not a new collection. The summaries endpoint (daily language/project breakdowns) is the highest-value pull with minimal request count. Wakapi (self-hosted WakaTime-compatible backend, Go/SQLite at ~/.local/share/wakapi/wakapi.db or configured path) is worth supporting as an alternative — same API surface, local DB readable directly via M3. Write to developer/wakatime/YYYY-MM.jsonl.

#### GitLab Activity (commits, MRs, issues) — _Developer Platform — Cloud_

🟢 **High — well-documented REST API, PAT-based, works for both gitlab.com and self-hosted instances. Same pattern as GitHub connector.** · M5 · OAuth / API key (PAT) · effort **S** · 🆕 new

- **Access:** REST API v4 at gitlab.com/api/v4/ (or self-hosted instance URL). PAT with read_api, read_user scopes. Key endpoints: GET /events (user activity feed), GET /projects?membership=true, GET /projects/{id}/repository/commits?author=, GET /merge_requests?scope=created_by_me, GET /issues?scope=created_by_me. Events API returns push/comment/MR/issue events.
- **Recommendation:** Build later — same effort as GitHub (S), same vault schema. Build as part of the same 'developer platforms' batch. Self-hosted support is a meaningful differentiator for enterprise users.
- **Notes:** The /events endpoint returns the last 100 events by default; use since/until params for incremental pulls. For full commit history, iterate /projects and /repository/commits per project. GitLab pagination uses X-Next-Page header. The self-hosted URL is user-configurable. Write to developer/gitlab/YYYY-MM.jsonl using the same schema as GitHub.

#### Alfred Clipboard History — _Clipboard Manager_

🟢 **High — plain SQLite, home dir, no special permissions, well-documented schema (confirmed by multiple independent sources including gist.github.com/pirate/6551e1c00a7c4b0c607762930e22804c). Alfred requires the Powerpack (~$34 one-time) for clipboard history, so not universally applicable.** · M3 · none (home-dir path, no FDA needed) · effort **S** · 🆕 new

- **Access:** SQLite database at ~/Library/Application Support/Alfred/Databases/clipboard.alfdb (find via Alfred Preferences → Advanced → Reveal in Finder → Databases/clipboard.alfdb). Table: clipboard(item TEXT, ts INTEGER, app TEXT, apppath TEXT, dataType INTEGER, dataHash TEXT). Supports text (dataType 0), images (dataType 2), file lists (dataType 8).
- **Recommendation:** Build later — useful for power users who already use Alfred with Powerpack. S effort once the M3 pattern is in place. Also cover Maccy (free, open source) as the accessible alternative.
- **Notes:** Alfred clipboard history retention is configurable (1 day to unlimited). Text items are stored as-is in the item column; images as binary blobs or file references. Maccy alternative: ~/Library/Application Support/Maccy/Maccy.sqlite, same general SQLite structure. Privacy: clipboard can contain passwords/secrets — add a configurable exclusion list (by app name, e.g. 1Password, Keychain) and never log clipboard items from security-flagged apps. Write to developer/clipboard/YYYY-MM.jsonl with fields: ts, text (truncated at configurable limit), app, data_type.

#### Qbserve App Time Tracker — _Productivity Tracking — Local_

🟡 **Medium — the DB path is confirmed and SQLite is readable, but the schema is undocumented and would require reverse-engineering. Not universally applicable (paid app, niche). The app tracks URLs via browser extensions (Firefox/Vivaldi/Opera/Yandex — notably not Chrome or Safari natively).** · M3 · none · effort **M** · 🆕 new

- **Access:** Local SQLite at ~/Library/Application Support/Qbserve/UserDatabase.sqlite. Daily backup at same dir as Backup.sqlite. No public API — data is fully local. CSV/timesheet export available via the app UI (format not documented, needs inspection). Qbserve is a one-time purchase app (no subscription) that tracks app/website usage locally.
- **Recommendation:** Spike first — confirm the schema is legible before committing to M effort. If the schema is simple (likely: app_name, bundle_id, duration_secs, ts, category), build it. Qbserve is privacy-respecting and local-only, making it a good Trove fit for existing users.
- **Notes:** The activity watcher already captures similar data natively; Qbserve integration is mainly useful to backfill historical data for users who already run Qbserve. Copy-then-read (app holds DB write lock while running). The 'Backup.sqlite' daily copy avoids the lock issue. Schema inspection needed: expected tables are something like 'records' (app, duration, day) and 'categories' (app → productivity category).

#### Timing App (macOS Time Tracker) — _Productivity Tracking — Local_

🟡 **Medium — data is locked behind a proprietary store with no direct DB path. Programmatic access requires AppleScript or the paid Web API (not all users have Expert plan). Timing is subscription-based (~$10/month). Not universally applicable.** · M6 · none (AppleScript) or localhost HTTP (Web API, Expert plan) · effort **M** · 🆕 new

- **Access:** Timing provides a Web API (localhost HTTP server, requires Expert plan or Timing Connect) and full AppleScript/JXA automation. AppleScript: tell application 'TimingHelper' … save report with settings → CSV/JSON/XLSX/HTML/PDF. The underlying data store path is not documented, but the AppleScript 'save report' command exports any date range to JSON. Also: Zapier integration for cloud push.
- **Recommendation:** Build later — M6 via AppleScript is pragmatic but the standalone constraint means Timing must be running. Implement as a scheduled M6 agent that runs 'save report' via AppleScript into the vault. Only valuable for existing Timing subscribers.
- **Notes:** The AppleScript 'save report' command can export to JSON format, making the output machine-readable. The agent approach: run an AppleScript that exports the last N days of activity to a temp JSON file, then Trove ingests it. The Timing Web API (localhost:10002) is cleaner but requires Expert plan. Timing tracks documents and URLs within apps (not just app names), giving richer context than the activity watcher. Write to developer/timing/YYYY-MM.jsonl. The M6 classification is correct: Timing must be running for the agent to call it.

#### macOS Download History (QuarantineEventsV2) — _File System Activity_

🟢 **High — home-dir SQLite, no permissions, records persist even after files are moved/deleted. Captures downloads from Safari, Chrome, Firefox, Mail, and any quarantine-aware app. Records the source URL — richer than just the Downloads folder listing.** · M3 · none (in ~/Library/Preferences, accessible without FDA) · effort **S** · 🆕 new

- **Access:** SQLite at ~/Library/Preferences/com.apple.LaunchServices.QuarantineEventsV2. Single table LSQuarantineEvent with columns: LSQuarantineEventIdentifier (UUID), LSQuarantineTimeStamp (seconds since 2001-01-01), LSQuarantineAgentBundleIdentifier (downloading app), LSQuarantineAgentName, LSQuarantineDataURLString (source URL), LSQuarantineOriginURLString (referrer), LSQuarantineOriginTitle, LSQuarantineSenderName, LSQuarantineSenderAddress, LSQuarantineTypeNumber.
- **Recommendation:** Build now — zero-permission S effort, surprisingly rich data (source URL, referring page, downloading app), persists after file deletion. A perfect complement to browser history that fills in 'what did I download and from where'.
- **Notes:** The timestamp is in Apple epoch (seconds since 2001-01-01 00:00:00 UTC), same as Safari's History.db — use the SAFARI_EPOCH_OFFSET_S constant already in browser.rs. The database is written by the OS quarantine system; Trove should only read it (copy-then-read). Records persist indefinitely (no auto-purge by macOS, but users can clear via Finder's 'Clear Downloads' or manually). Write to developer/downloads/YYYY-MM.jsonl with fields: ts, url, referrer_url, app_bundle_id, app_name, origin_title.

#### Screenshots Folder Metadata — _File System Activity_

🟢 **High — plain files, timestamp in filename, Spotlight tag confirms screenshot vs. other image. No OCR content capture (that is the explicitly iceboxed Rewind-style feature); just metadata (count, time-of-day distribution, file size).** · M3 · none (Desktop is accessible; custom locations may need FDA if inside protected dirs) · effort **S** · 🆕 new

- **Access:** Default location: ~/Desktop (macOS Mojave+) with filenames 'Screenshot YYYY-MM-DD at HH.MM.SS.png'. User-configurable via Screenshot app (Cmd+Shift+5). Metadata: creation timestamp embedded in filename + file system ctime/mtime. No GPS EXIF. macOS auto-tags screenshots with kMDItemIsScreenCapture=1 Spotlight attribute — queryable via NSMetadataQuery or mdfind.
- **Recommendation:** Build later — low effort but also low signal value (just 'I took N screenshots on this day'). Worth including as part of a broader 'file system activity' collector. Use mdfind to query kMDItemIsScreenCapture=1 for reliability across custom save locations.
- **Notes:** mdfind query: 'kMDItemIsScreenCapture == 1' returns all screenshots regardless of save location. For a custom location, Trove needs to know the path (user-configurable). File size as a rough content proxy (larger screenshot = more information captured). No OCR — the iceboxed Rewind-style approach is explicitly out of scope. Write to developer/screenshots/YYYY-MM.jsonl with fields: ts, filename, file_size_bytes, width, height (from PNG header, trivial to read).

#### Zed Editor AI Conversation History — _AI Session Transcripts_

🟡 **Medium — paths have changed across Zed versions (JSON → SQLite with compressed blobs), and there is no official documentation of the storage format. The GitHub discussion #32335 confirms threads.db exists but the blob format is undocumented. Zed is increasingly popular but the storage is in flux.** · M3 · none · effort **M** · 🆕 new

- **Access:** Historically stored at ~/.config/zed/conversations/*.json (JSON per conversation). Newer versions (2025+) store threads in ~/.local/share/zed/threads/threads.db (SQLite, compressed blobs). macOS path may use ~/.config/zed/ or XDG-style paths. Conversations can also be opened as Markdown files from the thread panel (manual export).
- **Recommendation:** Spike first — inspect threads.db schema before committing. If blobs are straightforward JSON/zstd, implement it. If heavily encoded, defer. Zed's userbase is growing fast among Rust/systems developers.
- **Notes:** The ~/.config/zed/conversations/*.json path (legacy) is straightforward and worth supporting as a fallback. The threads.db SQLite path is the future. Zed also stores editor state at ~/Library/Application Support/Zed/ on macOS (standard macOS app data dir). The AI improvement docs note conversations 'may be used for training' if feedback is sent — but local storage is local regardless.

#### Windsurf (Cascade) Chat History — _AI Session Transcripts_

🟡 **Medium — the path is confirmed by community sources but the exact file format inside the cascade/ directory is undocumented. The app is under active renaming/rebrand (Windsurf → Devin Desktop), which creates path instability risk.** · M3 · none · effort **M** · 🆕 new

- **Access:** Chat history at ~/.codeium/windsurf/cascade/ directory on macOS. Note: Windsurf was acquired by Cognition and rebranded as Devin Desktop (2026). The codeium/windsurf path prefix appears stable. GitHub issue #127 on Exafunction/codeium confirms lack of official export, but local files exist.
- **Recommendation:** Spike first — confirm the cascade/ directory format. If it is plaintext/JSON, S effort. The Windsurf → Devin Desktop rebrand may shift storage paths. Lower priority than Cursor (larger user base, better-documented storage).
- **Notes:** The GitHub issue #127 requesting chat history export was open with no response, suggesting the format is not officially supported. Community reverse-engineering will be needed. The VS Code-based global state.vscdb pattern may also apply (Windsurf is VS Code-based like Cursor).

#### GitHub Copilot Chat Sessions (VS Code) — _AI Session Transcripts_

🟢 **High — local JSON/JSONL files, no permissions, well-documented by the VS Code community. The kafumanto/copilot-tokens tool confirms the schema is legible. VS Code ≥1.109 format is the current standard.** · M3 · none · effort **S** · 🆕 new

- **Access:** VS Code workspace storage: ~/Library/Application Support/Code/User/workspaceStorage/<workspace-hash>/chatSessions/. Files are .json (flat snapshot) or .jsonl (append-only mutation log, VS Code ≥1.109). workspace.json in each hash dir identifies the repo/folder. Global index queryable by grepping workspace.json files for repo names.
- **Recommendation:** Build later — S effort, same infrastructure as VS Code recent workspaces. Natural batch with Cursor chat history. GitHub Copilot has a large user base making this broadly applicable.
- **Notes:** Each chatSessions/*.jsonl file is a mutation log for one conversation. The .json variant (older, flat snapshot) is also supported. Parse workspace.json files to map hash → repo path for context. Chat: Export Chat… VS Code command exports to the same format, so manual exports can be imported too (M1 fallback). Write to developer/copilot/YYYY-MM.jsonl with fields: ts, session_id, workspace_path, message_count, model (extracted from session metadata).

#### Bitbucket Activity (commits, PRs) — _Developer Platform — Cloud_

🟡 **Medium — Bitbucket's API is well-documented but less feature-rich than GitHub's (no user activity events feed); iterating repos to find commits is more expensive. Bitbucket market share has declined significantly; lower priority than GitHub/GitLab.** · M5 · OAuth / API key (App Password) · effort **S** · 🆕 new

- **Access:** Atlassian Bitbucket Cloud REST API v2 at api.bitbucket.org/2.0/. App password (user-specific token) or OAuth 2.0. Personal repos: /repositories/{username}. Commits: /repositories/{workspace}/{slug}/commits?author=. PRs: /pullrequests?q=author.uuid=. No user activity events feed comparable to GitHub's; must iterate repos. Self-hosted Bitbucket Data Center has its own REST API.
- **Recommendation:** Build later — lower market share than GitHub/GitLab, slightly more API work. Bundle with GitHub/GitLab as part of a 'developer platforms' batch.
- **Notes:** Bitbucket deprecated the REST API v1 fully; v2 is the current standard. The Bitbucket Cloud API uses cursor-based pagination (next URL in response). For personal use, App Passwords (account settings) are simpler than OAuth. Write to developer/bitbucket/YYYY-MM.jsonl using the same schema as GitHub/GitLab.

#### iTerm2 Command & Directory History — _Terminal & Shell_

🟡 **Medium — the storage path is not publicly documented and requires filesystem inspection. iTerm2 is popular among macOS developers but not universal. The shell history (~/.zsh_history) covers the same commands more accessibly.** · M3 · none · effort **M** · 🆕 new

- **Access:** iTerm2 Shell Integration stores per-session history and directory history. Files at ~/Library/Application Support/iTerm2/ (scripts, profiles) and ~/.iterm2_shell_integration.zsh (the hook). When 'Save copy/paste history and command history to disk' is enabled in Settings → General, per-user command history is stored internally. The exact DB path is not in official docs — likely under ~/Library/Application Support/iTerm2/ as a SQLite or plist file. Up to 200 commands per user/hostname retained.
- **Recommendation:** Icebox — shell history (~/.zsh_history) is a strictly better source for command history (no 200-command cap, timestamped, no app dependency). iTerm2's additional value (directory history, mark metadata) is niche. Only build if users specifically request it.
- **Notes:** iTerm2 shell integration also captures directory change history (cd events with timestamps) which .zsh_history doesn't record by default. This could be uniquely valuable but is a very small audience. The storage format investigation would require running find ~/Library/Application\ Support/iTerm2 on a machine with shell integration enabled.

#### macOS Recent Files (SFL2 / SharedFileList) — _File System Activity_

🟠 **Low — while the files are accessible, SFL2 format uses NSKeyedArchiver with opaque Bookmark data (no readable file paths). Parsing requires either a macOS Objective-C/Swift bridge or reverse-engineering the Bookmark binary format. The files are described as containing 'inscrutable UUIDs and chunks of gibberish text' (Eclectic Light Company). High implementation cost for data that largely overlaps with Spotlight's kMDItemLastUsedDate.** · M3 · none (~/Library/Application Support is accessible) · effort **L** · 🆕 new

- **Access:** Property list files at ~/Library/Application Support/com.apple.sharedfilelist/. Per-app recent docs: com.apple.LSSharedFileList.ApplicationRecentDocuments/<bundle-id>.sfl2. Global recents: com.apple.LSSharedFileList.RecentDocuments.sfl2, RecentApplications.sfl2, RecentServers.sfl2. SFL2 is NSKeyedArchiver binary plist format — contains Bookmark data (opaque binary), not plain paths. Parsing requires resolving Bookmarks to paths.
- **Recommendation:** Skip — use Spotlight metadata queries (kMDItemLastUsedDate via NSMetadataQuery or mdfind) instead, which return actual file paths without the NSKeyedArchiver complexity. The recent-files signal is better accessed via the Spotlight metadata index.
- **Notes:** Alternative: use mdfind with kMDItemLastUsedDate to get recently used files sorted by access time — this is a cleaner, path-based approach. Or parse the Spotlight database directly (~/Library/Metadata/CoreSpotlight/). The SFL2 approach is forensics tooling territory (AXIOM, macOS-artifact parsers) — not worth building from scratch in Rust.

#### ChatGPT Conversation Export — _AI Session Transcripts_

🟢 **High — official export, well-documented JSON format, stable since 2023. The export is periodic (manual trigger) rather than live, but covers complete history.** · M1 · none (one-shot import) · effort **S** · 🆕 new

- **Access:** Official export: Settings → Data Controls → Export → email ZIP containing conversations.json (complete history: all messages, timestamps, model info, metadata) and chat.html. Takes up to 7 days to arrive; download link expires in 24 hours. Not available for ChatGPT Business/Enterprise. Format: conversations.json is a JSON array of conversation objects with messages array.
- **Recommendation:** Build later — S effort M1 import. Natural complement to Claude Code history. The 7-day delay and manual trigger mean it is periodic backfill rather than live, but the data is valuable.
- **Notes:** conversations.json structure: array of {id, title, create_time (Unix epoch), update_time, mapping (dict of node_id → {message, parent, children})}. Each message has author.role (user/assistant/tool), content.parts (list of text strings), and create_time. Parse into developer/chatgpt/YYYY-MM.jsonl with fields: ts, conversation_id, title, role, content_preview (truncated), model (from metadata). Deduplicate by conversation_id + message_id across re-imports. Third-party browser extensions (AI Toolbox, ChatGPT Exporter) can export to Markdown/JSON instantly — worth noting as an alternative to waiting 7 days.

#### FSEvents File System Journal — _File System Activity_

🟠 **Low — requires root access to read /.fseventsd/ directly. While the Rust library exists, the forensic-level detail (every file system event) is extreme noise for a personal vault and the data volume is massive. The 'what files did I change' question is better answered by git activity (developer repos) and the QuarantineEvents DB (downloads).** · M3 · Full Disk Access (for /.fseventsd/ read) — requires root or FDA · effort **L** · 🆕 new

- **Access:** Binary gzip-compressed files at /.fseventsd/ on each APFS volume. Rust library: puffyCid/macos-fseventsd (available on crates.io). Records: file path, event type (create/modify/delete/rename), flags, event ID. Accessible via fsevent Rust crate (live API, M4) or the on-disk journal files (M3, requires root for /.fseventsd/ but user's home volume events are also in the journal).
- **Recommendation:** Skip — FDA + root requirement, massive noise-to-signal ratio for personal use. The targeted sources (git, downloads, recent files via Spotlight) answer the underlying question much better.
- **Notes:** The live FSEvents API (kqueue/FSEventStreamCreate) is accessible without root and fires callbacks for specific directories — this M4 approach could watch ~/Documents, ~/Desktop, ~/Downloads for file creation events. But the activity watcher already captures the app context, and the download history (QuarantineEventsV2) captures web-origin downloads specifically. A future 'file activity' collector watching specific dirs via kqueue would be an M-effort spike-first candidate.

#### JetBrains IDE Activity (IntelliJ, WebStorm, PyCharm, etc.) — _IDE Activity_

🟡 **Medium — the local history feature stores file-level edit events but in a proprietary binary format (not SQLite). Without a time-tracking plugin, there is no structured coding-time data. The WakaTime plugin for JetBrains sends data to the WakaTime API (covered above). Effort increases because format reverse-engineering would be needed.** · M3 · none · effort **M** · 🆕 new

- **Access:** Config: ~/Library/Application Support/JetBrains/<IDEName><Version>/. Logs: ~/Library/Logs/JetBrains/<IDEName><Version>/. Caches: ~/Library/Caches/JetBrains/<IDEName><Version>/. Local history (file edit timeline): in the system caches dir. No native time-tracking data unless a plugin like WakaTime, CodeStats, or the community Statistics plugin is installed. The Statistics plugin stores per-file/extension line counts but not timestamps.
- **Recommendation:** Icebox — without a time-tracking plugin installed, JetBrains IDEs don't store queryable activity logs. The WakaTime integration covers the time-tracking use case. Only revisit if a specific JetBrains-native data format (local history, workspace state) proves valuable.
- **Notes:** The JetBrains 'Local History' feature (VCS → Local History) stores file versions but the format is proprietary binary in the caches directory. The recently opened projects list is in ~/Library/Application Support/JetBrains/<IDE>/options/recentProjects.xml — XML, trivially parseable, gives 'what JetBrains projects did I open and when'. This recentProjects.xml S-effort piece might be worth including alongside VS Code workspaces.

### Computer & Developer Activity — cross-cutting notes

1. The M3 copy-then-read pattern (already proven for browser history, iMessage, Podcasts, Screen Time) applies to almost every source in this domain — VS Code/Cursor SQLite stores, Alfred clipboard.alfdb, Qbserve UserDatabase.sqlite, QuarantineEventsV2. All share the same rusqlite + WAL-copy approach already in browser.rs. The FDA grant troved already holds covers every file in ~/Library/ without additional prompts.

2. A single 'developer activity' vault subtree — developer/ — should house all sources in this domain, mirroring the correspondence/, tasks/, and music/ patterns. Suggested layout: developer/git/YYYY-MM.jsonl, developer/shell/YYYY-MM.jsonl, developer/github/commits/YYYY-MM.jsonl, developer/claude/YYYY-MM.jsonl, developer/cursor/YYYY-MM.jsonl, developer/downloads/YYYY-MM.jsonl.

3. Several sources (shell history, git activity, Claude Code transcripts, QuarantineEventsV2, VS Code workspaces, GitHub Copilot sessions) require zero additional permissions beyond what troved already holds, making them a zero-friction batch to build. The three S-effort, zero-permission sources — shell history, local git activity, Claude Code history — should be the first wave of this domain.

4. AI session transcript sources (Claude Code, Cursor, Zed, Windsurf, GitHub Copilot, ChatGPT) share a common vault schema and can be normalized under developer/ai-sessions/ with a source field. A shared ingest function that reads JSONL/JSON/SQLite and writes normalized session metadata (ts, source, project, model, message_count, summary) would make adding each new AI tool incremental.

5. GitHub/GitLab/Bitbucket all share the same vault output schema and can be built as one 'developer platforms' module with a per-provider config entry. The M1 archive export (GitHub account data ZIP, GitLab project export) is the right backfill strategy for all three, paired with M5 incremental pull for ongoing activity.

6. The privacy concern unique to this domain: clipboard history and shell history can contain secrets (API keys, passwords). Trove should implement a configurable exclusion list (by app name for clipboard, by command prefix for shell) and document the policy clearly. Raw vault data is kept complete; AI analysis should be explicitly gated before sending developer activity data to a cloud model.

7. Time-tracking apps (Timing, Qbserve, WakaTime) all partially overlap with the built activity watcher. The integration story should be: these are backfill sources for history before the Trove watcher was installed, and supplementary context (e.g., Timing's document-level tracking that the activity watcher can't do). Not replacements for the watcher.

---

## Web Activity & Content Consumption

This domain covers what users read, watch, save, annotate, and search on the web — a rich and high-value layer of personal data. The good news for Trove: the most popular sources have well-documented APIs or plain-file exports, and several important services (Pocket, Omnivore) have already shut down, so only historical one-shot imports remain relevant for them. The read-later and highlights ecosystem is particularly strong — Readwise/Reader, Instapaper, Raindrop, Pinboard, and Hypothesis all have stable token-based APIs that a Rust HTTP client can call directly. Google's ecosystem splits into two paths: Google Takeout (M1, one-shot, covers search and YouTube watch history) and the newer Google Data Portability API (M5, OAuth, potentially automatable). Local RSS readers (NetNewsWire, Reeder) store data in accessible local databases. The main gaps are services with no usable API and no export: Matter has no developer API, Substack exposes no reader-side data export, and Kagi deliberately doesn't store search history at all.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Readwise + Readwise Reader | Read-Later / Highlights | M5 | API token (user-generated at readwise.io/access_token) | S | 🟢 High — stable token-based REST API, well-documented, widely used, Rust HTTP call is all that's needed. Reader covers articles, PDFs, emails, RSS; Readwise covers Kindle, iBooks, and web highlights. Both support incremental sync via updatedAfter. | 🆕 new |
| Google Takeout — YouTube Watch + Search History | Search & Video History | M1 | Google account login; no special OAuth scope needed for user-initiated export | S | 🟢 High — JSON format is clean and parseable; the google-takeout-parser PyPI library documents the schema. YouTube watch history is NOT available via the YouTube Data API v3 (deprecated since 2017); Takeout is the only route. | 📋 planned |
| Google Data Portability API — My Activity | Search & Activity History | M5 | OAuth 2.0; requires app verification/security assessment before publication to general users | M | 🟡 Medium — API is live and documented, returns HTML+JSON activity archives. However, publishing a client app requires Google's security verification process (OAuth verification + security assessment). Usable as bring-your-own-credentials or during development; publishing to general users requires review. | 🆕 new |
| YouTube Data API v3 — Liked Videos, Playlists, Subscriptions | Video Activity | M5 | OAuth 2.0 (scope: youtube.readonly) | S | 🟢 High — watch history is gone from API but liked videos, playlists, and subscriptions are fully accessible via OAuth. These are complementary to Takeout history. | 🆕 new |
| Instapaper | Read-Later | M5 | OAuth 1.0a consumer key/secret (request at instapaper.com/main/request_oauth_consumer_token) plus xAuth for access token | M | 🟢 High — API is active and functional; multiple Rust/Python/Ruby client libraries exist. xAuth is unusual (not OAuth 2.0) but well-documented. Non-subscribers limited to 5 highlights/month stored. | 🆕 new |
| Raindrop.io | Bookmarks | M5 | OAuth 2.0 or personal API token | S | 🟢 High — well-documented REST API, active service, personal tokens available. Returns bookmark fields: id, url, title, tags, note, created, lastUpdate, collection, cover. | 🆕 new |
| Pinboard | Bookmarks | M5 | API token (no OAuth, just user:TOKEN in query param or header) | S | 🟡 Medium — API is alive and simple (keyless token, no OAuth dance). Service is in maintenance mode with slow response/limited dev activity as of 2024-2026, but still operational with 288M bookmarks stored. | 🆕 new |
| Hypothesis (Web Annotations) | Web Annotations | M5 | Personal API token (generated in Hypothesis account settings) | S | 🟢 High — clean REST API, personal dev tokens require no OAuth dance. Returns full annotation data: text, quote, target URI, tags, timestamps. Private groups supported with token. Open-source (AGPL), active project. | 🆕 new |
| NetNewsWire (Local RSS Reader) | RSS Reader (Local) | M3 | Full Disk Access (sandboxed container path) | M | 🟢 High — confirmed SQLite storage, open-source app (GitHub: Ranchero-Software/NetNewsWire), schema inspectable. Free app with large macOS user base. | 🆕 new |
| Reeder (Local RSS Reader) | RSS Reader (Local) | M3 | Full Disk Access | M | 🟡 Medium — Reeder 5 uses Realm DB (not SQLite), requiring a Rust Realm reader crate (realm-rs) or custom parser. Reeder Classic uses SQLite. Schema is undocumented but can be inspected. Paid app (~$9.99). | 🆕 new |
| Feedly | RSS Reader (Cloud) | M5 | Developer token (Pro plan required; token from account settings) | M | 🟡 Medium — API is fully functional and well-documented; covers feeds, boards (saved articles), read state. Blocker: requires Feedly Pro subscription for developer token. Can access up to 100 articles per request. | 🆕 new |
| Inoreader | RSS Reader (Cloud) | M5 | OAuth 2.0 or personal token (Pro plan required) | M | 🟡 Medium — functional API with good coverage of subscriptions, articles, starred items, read state. Blocked by Pro paywall. OPML export available for free. | 🆕 new |
| Pocket (Historical Import Only) | Read-Later (Defunct) | M1 | none (user must have their own export file) | S | 🟠 Low — service is dead and export window closed. Only users who exported before Nov 12, 2025 have data to import. No live API or export path exists. | 🆕 new |
| Omnivore (Historical Import Only) | Read-Later (Defunct) | M1 | none (user must have their own export ZIP) | S | 🟠 Low — hosted service gone, all data deleted. Self-hosted variants may exist but are edge case. Only users who exported before shutdown have data. | 🆕 new |
| Reddit (Saved Posts, Upvotes, Comments) | Social Bookmarks | M1 | Reddit account login for GDPR export; OAuth 2.0 + pre-approval for API | M | 🟡 Medium — GDPR export (M1) works well and is the recommended path; API is gated by new pre-approval requirement (2-4 week wait, as of Nov 2025). GDPR export has no 1000-item limit unlike the API. | 🆕 new |
| Hacker News (Saved/Submitted) | Social Bookmarks | M6 | none (public profile scrape) or account credentials for private favorites | M | 🟡 Medium — submitted stories/comments accessible via official API (no auth needed). Favorites require HTML scraping of the public favorites page (which only works if the user's submissions/favorites are public). No official export path. | 🆕 new |
| Safari Reading List (macOS) | Browser Read-Later (Local) | M3 | Full Disk Access (Safari container is protected) | S | 🟢 High — well-known path, binary plist parseable with plist Rust crates (plist crate on crates.io). Fields: URLString, URIDictionary (title), DateAdded, DateLastViewed, PreviewText. | 📋 planned |
| Substack (Reader Subscriptions) | Newsletter | M6 | Substack account credentials; no official API key | L | 🟠 Low — no official API, no reader-side export. Undocumented API endpoints are fragile. The subscription list (newsletters you follow) is not exported anywhere officially. | 🆕 new |
| Matter (Read-Later) | Read-Later | M1 | Matter account; manual export only | M | 🟠 Low — no public API. Manual export to Notion/Obsidian is available on Premium but requires app interaction. App is iOS-first with no documented macOS local data store. | 🆕 new |
| Wallabag (Self-Hosted Read-Later) | Read-Later (Self-Hosted) | M5 | OAuth 2.0 (client ID/secret from wallabag instance settings) | M | 🟡 Medium — excellent API with full-text article access and highlights, but requires user to be self-hosting Wallabag. Niche audience. | 🆕 new |
| Kagi Search History | Search History | M1 | N/A | XL | 🔴 Blocked — by design. Kagi's privacy model intentionally prevents history collection. This is a feature, not a bug, from their perspective. | 🆕 new |
| Wikipedia Contributions | Contributions | M5 | none (keyless public API) | S | 🟢 High — fully public API, no auth or API key needed, returns complete edit history. Only relevant for Wikipedia editors. | 🆕 new |
| Linkding / Shiori (Self-Hosted Bookmark Managers) | Bookmarks (Self-Hosted) | M5 | API token (generated in Linkding settings) | M | 🟡 Medium — excellent APIs for both tools, but requires users to be self-hosting. Niche but aligned with Trove's privacy-first audience. | 🆕 new |

### Detail

#### Readwise + Readwise Reader — _Read-Later / Highlights_

🟢 **High — stable token-based REST API, well-documented, widely used, Rust HTTP call is all that's needed. Reader covers articles, PDFs, emails, RSS; Readwise covers Kindle, iBooks, and web highlights. Both support incremental sync via updatedAfter.** · M5 · API token (user-generated at readwise.io/access_token) · effort **S** · 🆕 new

- **Access:** Readwise highlights API: GET https://readwise.io/api/v2/highlights/ (token from readwise.io/access_token). Reader documents API: GET https://readwise.io/api/v3/list/ with ?updatedAfter= for incremental sync. OPML export of feeds also available via account page.
- **Recommendation:** Build now — this is the richest highlights/read-later API available, covers both saved articles and annotated highlights in a single integration.
- **Notes:** Rate limits: Highlight LIST/Book LIST capped at 20 req/min; others 240 req/min. Highlights include parent_id linking them to source docs. Reader also offers a full ZIP export of article content via the account page. Two separate tokens/endpoints (readwise.io/api/v2 for Readwise, readwise.io/api/v3 for Reader) but same auth scheme.

#### Google Takeout — YouTube Watch + Search History — _Search & Video History_

🟢 **High — JSON format is clean and parseable; the google-takeout-parser PyPI library documents the schema. YouTube watch history is NOT available via the YouTube Data API v3 (deprecated since 2017); Takeout is the only route.** · M1 · Google account login; no special OAuth scope needed for user-initiated export · effort **S** · 📋 planned

- **Access:** takeout.google.com → select 'YouTube and YouTube Music' (format: JSON for watch-history.json with fields: title, titleUrl, subtitles[channel], time) and 'My Activity' → 'Search' (JSON gives timestamped queries). Can also select YouTube liked videos, subscriptions, playlists.
- **Recommendation:** Build now — YouTube watch history is high-value, well-structured JSON, and this is the only access path. Pair with the YouTube Data API for liked video metadata enrichment.
- **Notes:** Watch history JSON fields: title (prefixed 'Watched ...'), titleUrl (contains videoId param), subtitles[0].name (channel), time (ISO8601). Search history lives in a separate 'My Activity/Search' subfolder with similar structure. Takeout is one-shot; suggest periodic re-export reminder in UI. Search history chunks are named by date range.

#### Google Data Portability API — My Activity — _Search & Activity History_

🟡 **Medium — API is live and documented, returns HTML+JSON activity archives. However, publishing a client app requires Google's security verification process (OAuth verification + security assessment). Usable as bring-your-own-credentials or during development; publishing to general users requires review.** · M5 · OAuth 2.0; requires app verification/security assessment before publication to general users · effort **M** · 🆕 new

- **Access:** POST https://dataportability.googleapis.com/v1/portabilityArchive:initiate with resource myactivity.search (OAuth scope: https://www.googleapis.com/auth/dataportability.myactivity.search). Also supports myactivity.youtube, myactivity.maps, myactivity.play.
- **Recommendation:** Spike first — the API is more automatable than Takeout but requires OAuth app verification for public distribution. Consider for a v2 once Takeout M1 is shipping. Time-filter support makes incremental sync possible.
- **Notes:** Unlike Takeout (manual), this API can be called programmatically with user consent. Returns same data as Takeout but via an async archive-generation flow. Six activity resource groups available. The app verification requirement is the key blocker for general distribution; BYOC (bring-your-own-credentials) sidesteps it.

#### YouTube Data API v3 — Liked Videos, Playlists, Subscriptions — _Video Activity_

🟢 **High — watch history is gone from API but liked videos, playlists, and subscriptions are fully accessible via OAuth. These are complementary to Takeout history.** · M5 · OAuth 2.0 (scope: youtube.readonly) · effort **S** · 🆕 new

- **Access:** GET https://www.googleapis.com/youtube/v3/playlistItems?playlistId=LL (LL = liked videos special playlist ID) with OAuth token. GET /subscriptions, /playlists for user's playlists. 10,000 units/day quota; playlistItems.list costs 1 unit.
- **Recommendation:** Build now alongside YouTube Takeout — liked videos and subscription list are high-value personal data not in the Takeout export.
- **Notes:** Daily quota of 10,000 units is generous for personal use. Note: watch history was deprecated from the API in 2017; Takeout is the only route for that. The LL playlist ID is the liked-videos pseudo-playlist.

#### Instapaper — _Read-Later_

🟢 **High — API is active and functional; multiple Rust/Python/Ruby client libraries exist. xAuth is unusual (not OAuth 2.0) but well-documented. Non-subscribers limited to 5 highlights/month stored.** · M5 · OAuth 1.0a consumer key/secret (request at instapaper.com/main/request_oauth_consumer_token) plus xAuth for access token · effort **M** · 🆕 new

- **Access:** Full API at instapaper.com/api/full using xAuth (OAuth 1.0a, HMAC-SHA1). Key endpoints: /api/1/bookmarks/list, /api/1/highlights/list. Also: Settings → Export → Download CSV (up to 2000 articles, 4 columns: URL, title, selection, folder) or HTML.
- **Recommendation:** Build now — large installed base of Instapaper users, clean API. The CSV export (M1) is a zero-friction fallback if OAuth proves burdensome.
- **Notes:** xAuth requires sending username/password to get a token — slightly awkward UX. Consumer key requires registration with Instapaper. The CSV export path is simpler and covers the 2000 most recent saves. Highlights are API-only (not in CSV). Non-free subscribers get unlimited highlights.

#### Raindrop.io — _Bookmarks_

🟢 **High — well-documented REST API, active service, personal tokens available. Returns bookmark fields: id, url, title, tags, note, created, lastUpdate, collection, cover.** · M5 · OAuth 2.0 or personal API token · effort **S** · 🆕 new

- **Access:** REST API at developer.raindrop.io — GET https://api.raindrop.io/rest/v1/raindrops/{collectionId} with OAuth 2.0 token. Export: Settings → Backup → Export (HTML/CSV/TXT). API token from developer integrations section.
- **Recommendation:** Build now — Raindrop has a large user base post-Pocket shutdown and clean API. Personal token makes auth simple.
- **Notes:** API backup/export features may require paid plan (Pro). Free tier still has personal tokens for read access. CSV/HTML export is always available. raindrop-io-py Python library and others exist as reference implementations.

#### Pinboard — _Bookmarks_

🟡 **Medium — API is alive and simple (keyless token, no OAuth dance). Service is in maintenance mode with slow response/limited dev activity as of 2024-2026, but still operational with 288M bookmarks stored.** · M5 · API token (no OAuth, just user:TOKEN in query param or header) · effort **S** · 🆕 new

- **Access:** REST API at pinboard.in/api/ — GET https://api.pinboard.in/v1/posts/all?auth_token=user:TOKEN&format=json. Token found at pinboard.in/settings/password. Rate limit: 1 request/3 seconds. Also: pinboard.in/export/ for XML/JSON/Netscape HTML.
- **Recommendation:** Build now — trivially simple API (token in query string, JSON response), and there are dedicated users who have used Pinboard for 15+ years of bookmark history. Low implementation cost.
- **Notes:** Service reliability has declined; worth adding a health-check to the collector. Rate limit of 1 req/3s is slow for initial full sync of large archives. The /posts/all endpoint returns the full archive in one call (paginated for large accounts). API v2 draft exists but never shipped.

#### Hypothesis (Web Annotations) — _Web Annotations_

🟢 **High — clean REST API, personal dev tokens require no OAuth dance. Returns full annotation data: text, quote, target URI, tags, timestamps. Private groups supported with token. Open-source (AGPL), active project.** · M5 · Personal API token (generated in Hypothesis account settings) · effort **S** · 🆕 new

- **Access:** REST API at hypothes.is/api/ — GET https://api.hypothes.is/api/search?user=acct:USERNAME@hypothes.is with Authorization: Bearer TOKEN header. Token from hypothes.is/account/developer.
- **Recommendation:** Build now — niche but high-value for power users who annotate the web. Very simple token-based API, small response payloads.
- **Notes:** hypexport (GitHub: karlicoss/hypexport) is a reference implementation. Both public and private annotations accessible with token. Group annotations require knowing group ID but are discoverable via /api/profile/groups.

#### NetNewsWire (Local RSS Reader) — _RSS Reader (Local)_

🟢 **High — confirmed SQLite storage, open-source app (GitHub: Ranchero-Software/NetNewsWire), schema inspectable. Free app with large macOS user base.** · M3 · Full Disk Access (sandboxed container path) · effort **M** · 🆕 new

- **Access:** Local SQLite DB at ~/Library/Containers/com.ranchero.NetNewsWire-Evergreen/Data/Library/Application Support/NetNewsWire/Accounts/OnMyMac/. Contains article read state, feed metadata, article content.
- **Recommendation:** Build now — free, popular, local-only RSS reader. The only macOS-native RSS reader with a clean SQLite store and open-source schema. Read-only poll is straightforward with FDA.
- **Notes:** App is sandboxed so the container path requires Full Disk Access. Multiple account types (OnMyMac, Feedbin, Feedly) each get their own subfolder. OPML export also possible from the app itself (File → Export Subscriptions) as a simpler M1 fallback for feed list only.

#### Reeder (Local RSS Reader) — _RSS Reader (Local)_

🟡 **Medium — Reeder 5 uses Realm DB (not SQLite), requiring a Rust Realm reader crate (realm-rs) or custom parser. Reeder Classic uses SQLite. Schema is undocumented but can be inspected. Paid app (~$9.99).** · M3 · Full Disk Access · effort **M** · 🆕 new

- **Access:** Reeder 5 stores data in Realm DB at ~/Library/Containers/com.reederapp.5.macOS/Data/Library/Application Support/default.realm. Query starred items with 'starred == 1'. Earlier Reeder Classic uses SQLite at ~/Library/Containers/com.reederapp.macOS/Data/Library/Application Support/.
- **Recommendation:** Build later — worth supporting given Reeder's popularity on macOS, but Realm DB parsing is more complex than SQLite. Start with NetNewsWire (SQLite), add Reeder once that's shipping.
- **Notes:** Realm DB format: realm-rs crate exists in Rust ecosystem. Starred articles queryable. Reeder syncs with Feedbin/Feedly/iCloud so the local DB reflects the user's full reading history. Bundle ID varies by version: com.reederapp.5.macOS (Reeder 5), com.reederapp.macOS (Classic).

#### Feedly — _RSS Reader (Cloud)_

🟡 **Medium — API is fully functional and well-documented; covers feeds, boards (saved articles), read state. Blocker: requires Feedly Pro subscription for developer token. Can access up to 100 articles per request.** · M5 · Developer token (Pro plan required; token from account settings) · effort **M** · 🆕 new

- **Access:** REST API at developers.feedly.com — GET https://cloud.feedly.com/v3/streams/contents?streamId=... for articles; /v3/tags for saved/starred boards. Personal dev token from feedly.com/i/team/api (requires Feedly Pro, ~$72/yr).
- **Recommendation:** Build later — good API but Pro paywall limits addressable user base. Consider after NetNewsWire/Reeder local reads are done.
- **Notes:** API supports full OAuth for non-personal apps. For Trove, user brings their own dev token (Pro required). OPML export always available for feed subscriptions list without API access. Feedly free users cannot use the API.

#### Inoreader — _RSS Reader (Cloud)_

🟡 **Medium — functional API with good coverage of subscriptions, articles, starred items, read state. Blocked by Pro paywall. OPML export available for free.** · M5 · OAuth 2.0 or personal token (Pro plan required) · effort **M** · 🆕 new

- **Access:** API at inoreader.com/developers — GET https://www.inoreader.com/reader/api/0/subscription/list for feeds; /reader/api/0/stream/contents/... for articles. OAuth2 or personal token. API access requires Pro plan ($90/yr or $9.99/mo).
- **Recommendation:** Build later — lower priority than Feedly due to smaller user base; same Pro paywall caveat. OPML export (M1) gives feed list for free.
- **Notes:** API mirrors the Google Reader API pattern (familiar to developers). OPML export at Settings → Import/Export covers feed subscriptions without requiring Pro. Supports read state, tags, starred items via API.

#### Pocket (Historical Import Only) — _Read-Later (Defunct)_

🟠 **Low — service is dead and export window closed. Only users who exported before Nov 12, 2025 have data to import. No live API or export path exists.** · M1 · none (user must have their own export file) · effort **S** · 🆕 new

- **Access:** Pocket shut down July 8, 2025. Data export portal closed November 12, 2025. API disabled November 12, 2025. Any previously exported data can be ingested as JSON/HTML.
- **Recommendation:** Build now (trivial M1) — many former Pocket users have export files. Parse Pocket's HTML or CSV export format to ingest historical saves. Low effort, helps a large cohort of displaced users.
- **Notes:** Pocket export format is HTML (Netscape bookmarks format) with URL, title, tags, timestamp, read status. The export file is self-contained. This is purely historical archival — no live sync possible.

#### Omnivore (Historical Import Only) — _Read-Later (Defunct)_

🟠 **Low — hosted service gone, all data deleted. Self-hosted variants may exist but are edge case. Only users who exported before shutdown have data.** · M1 · none (user must have their own export ZIP) · effort **S** · 🆕 new

- **Access:** Omnivore shut down November 2024 after ElevenLabs acquihire. All hosted data deleted. Self-hosted instances still possible via GitHub (AGPL). For former users who exported: Omnivore export was a ZIP of markdown files per article.
- **Recommendation:** Build now (trivial) — accept Omnivore markdown export ZIP. Small but passionate user base who likely exported. The markdown-per-article format is easy to parse.
- **Notes:** Omnivore export ZIP contains one markdown file per saved article with frontmatter (URL, title, tags, highlights as blockquotes). GitHub repo at github.com/omnivore-app/omnivore under AGPL if self-hosted variants need consideration.

#### Reddit (Saved Posts, Upvotes, Comments) — _Social Bookmarks_

🟡 **Medium — GDPR export (M1) works well and is the recommended path; API is gated by new pre-approval requirement (2-4 week wait, as of Nov 2025). GDPR export has no 1000-item limit unlike the API.** · M1 · Reddit account login for GDPR export; OAuth 2.0 + pre-approval for API · effort **M** · 🆕 new

- **Access:** Official GDPR export: reddit.com/prefs/data → 'Download my data' → ZIP with JSON files for saved, upvoted, downvoted, comments, submissions. API path (for existing creds): OAuth 2.0 personal script app, GET /user/{username}/saved.json (limited to 1000 items). New API apps require pre-approval (as of Nov 2025).
- **Recommendation:** Build now using M1 (GDPR export) — the JSON export covers saved posts, upvoted content, comments, and submissions without API approval. Add M5 API path as optional enhancement for users who already have API credentials.
- **Notes:** GDPR export ZIP contains: saved.json, upvoted.json, downvoted.json, comments.json, posts.json. API alternative uses the PRAW-style OAuth flow but new app registrations now require pre-approval. rexport (github.com/karlicoss/rexport) is a good reference for both paths.

#### Hacker News (Saved/Submitted) — _Social Bookmarks_

🟡 **Medium — submitted stories/comments accessible via official API (no auth needed). Favorites require HTML scraping of the public favorites page (which only works if the user's submissions/favorites are public). No official export path.** · M6 · none (public profile scrape) or account credentials for private favorites · effort **M** · 🆕 new

- **Access:** Official Firebase API: GET https://hacker-news.firebaseio.com/v0/user/{username}.json (returns submitted item IDs only; no favorites endpoint). Favorites/upvotes are not in the public API. Scraping workaround: https://news.ycombinator.com/favorites?id={username} (HTML scrape, public if profile is public).
- **Recommendation:** Build later — niche but valued by HN power users. Implement as M6 (agent/scraper) that reads news.ycombinator.com/favorites?id=USERNAME and news.ycombinator.com/submitted?id=USERNAME. Low engineering cost.
- **Notes:** The official Firebase API (hacker-news.firebaseio.com) has no favorites endpoint — this is confirmed by Y Combinator. Submitted items (stories + comments) are accessible without auth. Favorites page is paginated HTML. github.com/reactual/hacker-news-favorites-api and github.com/kisabaka/hackernews-stories are reference scrapers.

#### Safari Reading List (macOS) — _Browser Read-Later (Local)_

🟢 **High — well-known path, binary plist parseable with plist Rust crates (plist crate on crates.io). Fields: URLString, URIDictionary (title), DateAdded, DateLastViewed, PreviewText.** · M3 · Full Disk Access (Safari container is protected) · effort **S** · 📋 planned

- **Access:** Stored in ~/Library/Safari/Bookmarks.plist (binary plist, Reading List entries alongside bookmarks). Also ~/Library/Safari/ReadingListArchives/ for cached page content. Can be parsed with macOS plist APIs or plutil.
- **Recommendation:** Build now — cheap to implement (same FDA permission needed for Safari history which is already built), gives natural complement to Safari history.
- **Notes:** Already in 'planned' bucket per brief. The Bookmarks.plist is binary plist; plist crate handles it. Reading List items are nested under a 'com.apple.ReadingList' WebBookmarkType node. iCloud syncs this so it reflects cross-device saves.

#### Substack (Reader Subscriptions) — _Newsletter_

🟠 **Low — no official API, no reader-side export. Undocumented API endpoints are fragile. The subscription list (newsletters you follow) is not exported anywhere officially.** · M6 · Substack account credentials; no official API key · effort **L** · 🆕 new

- **Access:** No public API for reader-side data. Substack's official data export (Settings → Export data) covers only your own posts, comments, and subscriber list if you're a publisher — NOT the newsletters you subscribe to or your reading history. Undocumented endpoints exist (discovered via browser DevTools) but are unofficial.
- **Recommendation:** Icebox — no stable access path for reader data. Monitor for official API. Workaround: users can manually export email archive via Gmail/IMAP (newsletters arrive as emails) which is covered by the email domain.
- **Notes:** The browser DevTools approach to Substack's unofficial API is documented by community developers but violates ToS and is fragile. The best practical approach for Substack reading data is to capture the newsletters via the email/IMAP integration rather than Substack directly.

#### Matter (Read-Later) — _Read-Later_

🟠 **Low — no public API. Manual export to Notion/Obsidian is available on Premium but requires app interaction. App is iOS-first with no documented macOS local data store.** · M1 · Matter account; manual export only · effort **M** · 🆕 new

- **Access:** No public developer API documented anywhere. Matter II (hq.getmatter.com) has no API documentation page. Premium ($60/yr) includes Notion/Obsidian export but no programmatic API. iOS-focused app.
- **Recommendation:** Icebox — no API and no local data path. If Matter adds an API, revisit. A significant user base exists post-Pocket shutdown so worth monitoring.
- **Notes:** Matter's Notion export (Premium) could be used as an indirect M1 path if the user exports to Notion then Trove reads from there, but that's a multi-hop workaround. Matter has repeatedly stated it's iOS-first and has no macOS app.

#### Wallabag (Self-Hosted Read-Later) — _Read-Later (Self-Hosted)_

🟡 **Medium — excellent API with full-text article access and highlights, but requires user to be self-hosting Wallabag. Niche audience.** · M5 · OAuth 2.0 (client ID/secret from wallabag instance settings) · effort **M** · 🆕 new

- **Access:** REST API at {instance}/api/ — GET /api/entries.json with OAuth2 (client_credentials grant). Returns full article text, highlights, tags, reading status. Also hosted at wallabag.it for ~11 EUR/yr.
- **Recommendation:** Build later — smaller user base than Readwise/Instapaper but the self-hosted angle appeals to privacy-conscious Trove users. API is clean and well-documented.
- **Notes:** Wallabag v2.6.14 as of 2026. Stores full article text locally. Highlights exposed via API. Import from Pocket/Instapaper/Readability. The wallabag.it hosted option means not all users are self-hosting. PHP/Symfony stack.

#### Kagi Search History — _Search History_

🔴 **Blocked — by design. Kagi's privacy model intentionally prevents history collection. This is a feature, not a bug, from their perspective.** · M1 · N/A · effort **XL** · 🆕 new

- **Access:** Not available. Kagi explicitly does not associate search queries with user accounts. Queries are only temporarily logged for debugging and auto-purged. No export, no API for personal search history.
- **Recommendation:** Skip — unsolvable by design. Kagi's privacy-first model means no search history exists server-side. Trove users who care about search history should note this gap.
- **Notes:** Kagi does offer a Search API ($0.012/query) for running searches programmatically, but that's entirely different from retrieving history. No workaround exists short of a browser extension intercepting searches at query time.

#### Wikipedia Contributions — _Contributions_

🟢 **High — fully public API, no auth or API key needed, returns complete edit history. Only relevant for Wikipedia editors.** · M5 · none (keyless public API) · effort **S** · 🆕 new

- **Access:** MediaWiki Action API: GET https://en.wikipedia.org/w/api.php?action=query&list=usercontribs&ucuser={username}&format=json — returns edits with timestamp, page title, diff size, comment. No auth needed for public contributions. Paginate with uccontinue.
- **Recommendation:** Build later — niche (only valuable for Wikipedia editors), but trivially easy to implement (no auth, simple GET). Low implementation cost justifies inclusion.
- **Notes:** API returns edit metadata but not the full diff text by default (add &ucprop=ids|title|timestamp|comment|sizediff for useful fields). Works across all MediaWiki wikis by changing the domain. Wikidata contributions accessible at the same endpoint on wikidata.org.

#### Linkding / Shiori (Self-Hosted Bookmark Managers) — _Bookmarks (Self-Hosted)_

🟡 **Medium — excellent APIs for both tools, but requires users to be self-hosting. Niche but aligned with Trove's privacy-first audience.** · M5 · API token (generated in Linkding settings) · effort **M** · 🆕 new

- **Access:** Linkding REST API: GET http://{instance}/api/bookmarks/ with Token auth header. SQLite DB at /etc/linkding/data/db.sqlite3 inside Docker volume. Shiori also has REST API + SQLite at /srv/shiori/data/shiori.db.
- **Recommendation:** Build later — fits Trove's privacy-first ethos perfectly (self-hosting users are exactly Trove's audience). Linkding's API is clean. Low implementation effort once M5 HTTP infrastructure is in place.
- **Notes:** Linkding Docker volume persists SQLite at /etc/linkding/data/db.sqlite3 — if running locally on the same Mac could also be M3. REST API is the cleaner path. Linkding stores snapshots of bookmarked pages as local HTML. Shiori is an alternative with similar capability.

### Web Activity & Content Consumption — cross-cutting notes

1. TOKEN AUTH DOMINATES: Almost all cloud services in this domain use simple bearer token auth (Readwise, Raindrop, Pinboard, Hypothesis) or OAuth 2.0 (Feedly, Inoreader, Instapaper/xAuth). A single reusable Rust HTTP client + token store covers most of them. Build a shared `TokenStore` in trove-core that all M5 collectors in this domain can share.

2. DEAD SERVICES = HISTORICAL IMPORT OPPORTUNITY: Pocket (shut down July 2025) and Omnivore (shut down Nov 2024) left large user bases. Adding M1 import parsers for their export formats is low effort and high goodwill. Pocket's format is Netscape HTML bookmarks; Omnivore's is markdown ZIP.

3. LOCAL RSS READERS = FDA GATE: Both NetNewsWire (SQLite) and Reeder (Realm DB) live behind Full Disk Access. Since Trove already needs FDA for other collectors (iMessage, Safari history), this permission is already in the expected grant set — no new permission prompt needed beyond what's already asked.

4. THE GOOGLE PROBLEM — TWO PATHS: YouTube watch history is Takeout-only (M1; no API). YouTube liked videos/subscriptions are API-only (M5; watch history was deprecated from API in 2017). Search history is Takeout (M1) or Data Portability API (M5, requires OAuth app verification). Recommend shipping Takeout M1 first, then adding Data Portability API as an automatable upgrade path.

5. HIGHLIGHTS ECOSYSTEM CONVERGENCE: Readwise is the hub — it ingests highlights from Kindle, iBooks, Instapaper, Pocket, and Reader. A Trove user with Readwise likely already has their highlights centralized there. Build Readwise/Reader first; add Instapaper and Hypothesis as supplements for users not using Readwise.

6. PAYWALL GATES ON RSS CLOUD SERVICES: Both Feedly and Inoreader require paid plans to access their APIs. This limits addressable audience. Prioritize the free-tier local readers (NetNewsWire, Reeder) and OPML export (M1, always free) for feed subscription lists.

7. NO-API DEAD ENDS: Matter and Substack have no usable reader-side API or export. For Substack, the practical workaround is to capture newsletter emails via the Gmail/IMAP integration (already in Trove's planned scope). For Matter, monitor for future API.

8. SELF-HOSTED TOOLS ALIGN WITH TROVE AUDIENCE: Users of Wallabag, Linkding, or Shiori are exactly the privacy-conscious self-hosters who would also run Trove. These are worth building despite small user bases — they're high-signal audience fits.

---

## Home, IoT & Smart Devices

The connected-home domain splits cleanly into three tiers: local-first sources (HomeKit's homed SQLite, Hue's LAN bridge, Tempest UDP, Enphase IQ Gateway LAN, BLE sensors), cloud-pull sources that are straightforward OAuth M5 integrations (Google Nest SDM, Honeywell Resideo, Moen Flo, WeatherFlow REST, Ambient Weather, Tesla Fleet API, Ring reversed API, Emporia/Sense), and gated or blocked sources (Ecobee — developer registration closed; Alexa — export only; Home Assistant — requires HA running, violates standalone rule unless user already self-hosts). The overall feasibility for Trove is high for the first two tiers; the main pattern is: local sensors without a cloud API (Aranet4, Govee BLE, Hue) read via BLE/LAN, cloud-tied devices via OAuth pulls, and utility/energy data via Green Button standard download or CSV. A single smart-home vault schema (JSONL per device, per day) can cover nearly everything in this domain.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Apple HomeKit (homed local DB) | Smart Home Hub | M3 | FDA (the path is user-space Library, likely accessible without FDA in practice, but FDA covers edge cases) | M | 🟢 High — database is local, well-documented by community reverse-engineering (github.com/tamengual/homekit-extractor), and readable with rusqlite. No API key or cloud call. Content: device list, room/zone/scene structure, automation definitions. Limitation: no historical state change log — only current config and automation rules. | 🆕 new |
| Philips Hue (local bridge API) | Smart Lighting | M5 | none (LAN HTTP, no macOS TCC) | S | 🟢 High — fully documented official local API (developers.meethue.com), no cloud dependency, no rate limits for personal use. Current state for all lights, sensors (motion, temperature, daylight), rooms, zones, and scenes. Caveat: no historical state log — only real-time polling. | 🆕 new |
| WeatherFlow Tempest (personal weather station) | Personal Weather Station | M5 | none (LAN UDP or outbound HTTPS) | S | 🟢 High — official published UDP spec (v171 current), real-time local data with no account required when on same network. Cloud API adds historical data and forecast. Station owner can query their own data only. | 🆕 new |
| Enphase Solar (IQ Gateway local + cloud API) | Solar / Energy | M5 | none (LAN HTTPS or outbound HTTPS) | M | 🟢 High — Enphase officially documents both the local gateway API and the cloud Enlighten API. Local path gives real-time production data. Cloud path gives historical calendar data. Widely used by community (pypowerwall, Matthew1471/Enphase-API on GitHub). | 🆕 new |
| Google Nest (Smart Device Management API) | Smart Thermostat / Cameras | M5 | OAuth (Google account + $5 Device Access sandbox fee) | M | 🟡 Medium — API is live and well-documented; $5 registration and Gmail-only requirement are minor friction. Key limitation: API returns only CURRENT state, not historical data. Temperature/humidity/HVAC status must be logged by Trove's own polling. No history endpoint exists. Pub/Sub for live event delivery. | 🆕 new |
| Honeywell / Resideo Thermostat (developer.honeywellhome.com) | Smart Thermostat | M5 | OAuth (Honeywell developer registration) | M | 🟡 Medium — developer portal is live as of 2026, registration open, API functional. Returns current temperature, setpoint, mode, humidity, fan status. No historical data endpoint — same limitation as Nest; Trove must poll and persist. | 🆕 new |
| Ambient Weather (personal weather station) | Personal Weather Station | M5 | API key (bring-your-own from ambientweather.net account) | S | 🟢 High — official documented REST API (ambientweather.docs.apiary.io), no OAuth friction, simple key-based auth. Data: outdoor temp/humidity/wind/rain/UV/solar, indoor temp/humidity, hourly/daily summaries. | 🆕 new |
| Aranet4 CO2 / Air Quality (BLE local) | Indoor Air Quality | M4 | TCC: Bluetooth (CoreBluetooth entitlement for Tauri app) | M | 🟡 Medium — BLE protocol is community-documented, Rust library exists. macOS pairing sometimes finicky on M-series Macs (known forum complaints). Sensor must be physically in Bluetooth range at collection time. No cloud dependency at all — fully local. | 🆕 new |
| Awair Element (indoor air quality — local API + cloud) | Indoor Air Quality | M5 | none for local LAN HTTP; OAuth for cloud API | S | 🟢 High — official Awair local API feature documented (support.getawair.com/hc/en-us/articles/360049221014). Rust crate exists (blog.yossarian.net/2023/03/20/Introducing-awair-local-api-rs). Data export to CSV also available from Awair dashboard. | 🆕 new |
| Airthings Wave / View (indoor air quality + radon) | Indoor Air Quality | M5 | OAuth (Airthings developer portal — free account) | S | 🟢 High — official documented consumer API, free registration, personal device data. Data: radon (Bq/m3), CO2, VOC, PM, temp, humidity, pressure. Dashboard CSV export available as M1 fallback. | 🆕 new |
| Sense Energy Monitor | Home Energy Monitor | M5; M1 fallback | Account credentials (unofficial); CSV export = none | M | 🟡 Medium — no official API (community has requested it for years with no response). Unofficial library works but could break on API changes. CSV export is stable but manual. Device-level usage detection (always-on, fridge, EV charger, etc.) is valuable. | 🆕 new |
| Emporia Vue (home energy monitor) | Home Energy Monitor | M5 | Account credentials (unofficial) | M | 🟡 Medium — Emporia explicitly acknowledges pyemvue as community-supported but unsupported by them. All data flows through Emporia cloud. 5-second resolution available via unofficial API; 1-minute and hourly aggregates. Ported to Rust possible via HTTP client. | 🆕 new |
| Tesla Powerwall + Solar (local gateway + Fleet API) | Solar / Battery Storage | M5 | Account credentials (local) or OAuth (Fleet API — requires Tesla developer account) | L | 🟡 Medium — local API is unofficial/undocumented, affected by firmware updates (as of FW 25.10.0 TEDAPI LAN routing removed — must connect to Powerwall Wi-Fi AP directly). Fleet API is official but requires developer registration and Tesla account. pypowerwall (Python) and vloschiavo/powerwall2 community docs cover both paths. | 🆕 new |
| Ring (doorbell / camera event logs) | Security / Doorbell | M5 | Account credentials (unofficial) | M | 🟡 Medium — unofficial API actively maintained as of Feb 2026 (v0.9.14 release). Provides event history, device health, alert metadata. Risk: Ring (Amazon) can change private API at any time. No official API and no export feature. | 🆕 new |
| Home Assistant (self-hosted hub) | Smart Home Hub | M5 | none (local HTTP; user must have HA running and provide token) | M | 🟡 Medium — API is well-documented and stable. CRITICAL STANDALONE CONSTRAINT: Home Assistant must be running for the API to work. Trove cannot require HA to be running. However: if a user self-hosts HA, treating it as an always-on data source that Trove polls opportunistically (not requiring it) is acceptable. Rich history data covering ALL home devices HA knows about. | 🆕 new |
| Netatmo Personal Weather Station | Personal Weather Station | M5 | OAuth (Netatmo developer app registration — free) | S | 🟢 High — official documented API, well-supported (pyatmo Python library, PHP SDK, Go CLI). Historical data queryable with date range. No data retention limit mentioned. | 🆕 new |
| Moen Flo Smart Water Monitor | Water Monitoring | M5 | Account credentials (unofficial) | M | 🟡 Medium — integration is live in Home Assistant (maintained), unofficial but stable. Water usage data is unique personal data not available elsewhere. No local API; device is cloud-dependent. | 🆕 new |
| Utility Smart Meter (Green Button) | Energy / Utility | M1 | none (manual download from utility account) | M | 🟡 Medium — Green Button DMD (download) is supported by most major US utilities. CMD (API) is rarely supported and requires utility-specific registration. XML/ESPI format needs parsing. Data: hourly or 15-min electricity usage (kWh), sometimes gas. | 🆕 new |
| Roborock Robot Vacuum (cleaning history + maps) | Robot Vacuum | M5 | none for local LAN (if token obtained); cloud auth needed to retrieve token | M | 🟡 Medium — local protocol is community-documented via python-miio (widely used). Cleaning history includes: start time, duration, area cleaned, error code, map snapshot. Roborock's official API is closed; iRobot filed Chapter 11 bankruptcy Dec 2025 (brand uncertain). | 🆕 new |
| August / Yale Smart Lock (entry logs) | Smart Lock | M5 | Account credentials (unofficial) | M | 🟡 Medium — yalexs is the reference implementation used by Home Assistant. Undocumented API can change. Entry log is valuable security + presence data. No local API (requires Yale cloud). | 🆕 new |
| TP-Link Kasa Smart Plugs (energy monitoring) | Smart Plugs / Energy | M5 | none (LAN TCP/UDP) | S | 🟡 Medium — local protocol documented and stable for older models (HS110, KP115). Newer Tapo-branded devices (TP100, P110) use a different encrypted protocol; python-kasa now supports both Kasa and Tapo. Some firmware versions have removed local API — check model-specific status. | 🆕 new |
| SwitchBot (hub + sensors) | Smart Home / Sensors | M5 | API key (bring-your-own from SwitchBot app) | S | 🟢 High — official published API with HMAC auth, well-documented (github.com/OpenWonderLabs/SwitchBotAPI). BLE devices require SwitchBot Hub to bridge to cloud. Temperature, humidity, motion, contact events all queryable. | 🆕 new |
| Ecobee Smart Thermostat | Smart Thermostat | M5 | OAuth (blocked for new registrations) | L | 🟠 Low — new developer accounts not being accepted as of June 2026 with no stated timeline for reopening. Existing integrations work but Trove cannot onboard new users with this path. HomeKit fallback only provides config data, not telemetry. | 🆕 new |
| Amazon Alexa (voice history export) | Voice Assistant | M1 | none (Amazon account login) | S | 🟡 Medium — export works but is manual and asynchronous (24-72hr). Audio recordings excluded. As of March 2025, Amazon disabled 'Do Not Send Voice Recordings' — all Alexa+ interactions go to cloud. Smart home device event logs NOT included in export. Only voice command transcriptions. | 🆕 new |
| Lutron Caséta (smart lighting / shades) | Smart Lighting | M5 | none (LAN TCP — PRO bridge required for local access) | M | 🟡 Medium — LEAP protocol local access works well on Smart Bridge PRO but requires the PRO model ($70 more than standard bridge). No history stored in bridge — only current state; Trove must poll and accumulate. pylutron-caseta is actively maintained. | 🆕 new |

### Detail

#### Apple HomeKit (homed local DB) — _Smart Home Hub_

🟢 **High — database is local, well-documented by community reverse-engineering (github.com/tamengual/homekit-extractor), and readable with rusqlite. No API key or cloud call. Content: device list, room/zone/scene structure, automation definitions. Limitation: no historical state change log — only current config and automation rules.** · M3 · FDA (the path is user-space Library, likely accessible without FDA in practice, but FDA covers edge cases) · effort **M** · 🆕 new

- **Access:** ~/Library/HomeKit/core.sqlite — CoreData SQLite managed by the homed daemon. Tables include home config, accessories, rooms, scenes, automations, and full Shortcut/workflow action data. No TCC prompt needed beyond normal Full Disk Access (FDA) since it lives in the user's own Library.
- **Recommendation:** Build now — zero-dependency local read; captures the user's full home topology and automation logic as a one-shot or polled snapshot.
- **Notes:** Does NOT contain per-device state history (no telemetry log). Only schema/config. The protobuf/NSKeyedArchiver nesting in automation data requires multi-layer decode. Devices use a UUID namespace isolated from HAP characteristic IDs — name-based matching only. M3 read via rusqlite; open in read-only mode to avoid locking homed.

#### Philips Hue (local bridge API) — _Smart Lighting_

🟢 **High — fully documented official local API (developers.meethue.com), no cloud dependency, no rate limits for personal use. Current state for all lights, sensors (motion, temperature, daylight), rooms, zones, and scenes. Caveat: no historical state log — only real-time polling.** · M5 · none (LAN HTTP, no macOS TCC) · effort **S** · 🆕 new

- **Access:** HTTPS to https://<bridge-ip>/api/ (v1) or https://<bridge-ip>/clip/v2/ (v2). Bridge discovered via mDNS or https://discovery.meethue.com. Auth: press physical link button once, POST to /api to receive a username token. All data stays LAN-local — no cloud required after initial setup.
- **Recommendation:** Build now — purely local, zero-friction auth, well-documented, clean JSON. Poll on a schedule for a lightweight presence/lighting log.
- **Notes:** API v2 (CLIP v2) uses SSE push events for state changes — better than polling for live capture. No history endpoint; state must be captured by Trove's own polling. Self-signed TLS cert on bridge — Rust reqwest needs accept_invalid_certs(true) or ship the bridge's cert. Hue Motion sensor exposes temperature + motion data (useful for presence detection).

#### WeatherFlow Tempest (personal weather station) — _Personal Weather Station_

🟢 **High — official published UDP spec (v171 current), real-time local data with no account required when on same network. Cloud API adds historical data and forecast. Station owner can query their own data only.** · M5 · none (LAN UDP or outbound HTTPS) · effort **S** · 🆕 new

- **Access:** Two paths: (1) UDP broadcast port 50222 on local LAN — hub broadcasts all sensor readings every few seconds, no auth needed; (2) REST + WebSocket cloud API at weatherflow.github.io/Tempest/api/ — personal access token from tempestwx.com Settings → Data Authorizations → Create Token.
- **Recommendation:** Build now — dual path (UDP for real-time, REST for history backfill) makes this a model integration. Complements the existing Open-Meteo weather source with hyper-local readings.
- **Notes:** UDP gives wind speed/direction, rain, lightning, temperature, humidity, UV, lux every 1–60s. Hub stores ~1 week locally on power. REST API history path: GET /v1/observations/station/{station_id}. Station ID from account. Personal token = bring-your-own; Trove should prompt once. Niche (Tempest owners only) but high data richness.

#### Enphase Solar (IQ Gateway local + cloud API) — _Solar / Energy_

🟢 **High — Enphase officially documents both the local gateway API and the cloud Enlighten API. Local path gives real-time production data. Cloud path gives historical calendar data. Widely used by community (pypowerwall, Matthew1471/Enphase-API on GitHub).** · M5 · none (LAN HTTPS or outbound HTTPS) · effort **M** · 🆕 new

- **Access:** Local: HTTPS to IQ Gateway (Envoy) at its LAN IP — token-based auth since firmware 7.0.x; 1-year token for system owner; generate at enphase.com or programmatically. Endpoints: /api/v1/production, /api/v1/production/inverters, etc. Cloud: Enlighten API v4 at developer-v4.enphase.com — OAuth, free Watt tier for personal system.
- **Recommendation:** Build now — solar production is high-value personal data with no equivalent source. Local gateway avoids rate limits. Good dual-path story: LAN poll + cloud backfill.
- **Notes:** Local token valid 1 year, needs refresh. Self-signed TLS on gateway. Cloud API free Watt tier: site-level production + consumption. Kilowatt tier adds microinverter-level data. Local API endpoint list is community-documented (github.com/Matthew1471/Enphase-API). No official public Enphase local API docs — but technically stable and widely deployed.

#### Google Nest (Smart Device Management API) — _Smart Thermostat / Cameras_

🟡 **Medium — API is live and well-documented; $5 registration and Gmail-only requirement are minor friction. Key limitation: API returns only CURRENT state, not historical data. Temperature/humidity/HVAC status must be logged by Trove's own polling. No history endpoint exists. Pub/Sub for live event delivery.** · M5 · OAuth (Google account + $5 Device Access sandbox fee) · effort **M** · 🆕 new

- **Access:** REST at https://smartdevicemanagement.googleapis.com/v1. OAuth 2.0 — user consents in Google's PCM flow. One-time $5 registration fee per developer account at console.nest.google.com. Supported devices: Nest thermostats, cameras, displays, doorbells.
- **Recommendation:** Build later — high-value for Nest owners but medium effort (OAuth + $5 + no history = requires ongoing polling). The $5 sandbox fee means Trove can compile in app credentials OR prompt BYOC. Poll every 5 min and write to JSONL for self-built history.
- **Notes:** API only returns current traits. Pub/Sub delivers events (connectivity, mode change, HVAC status). Refresh token expires in 6 months without use — must keep alive. API is live as of June 2026. Historical data (10-day only) shown in the Nest app is NOT queryable via API. Trove must build its own history from repeated polls.

#### Honeywell / Resideo Thermostat (developer.honeywellhome.com) — _Smart Thermostat_

🟡 **Medium — developer portal is live as of 2026, registration open, API functional. Returns current temperature, setpoint, mode, humidity, fan status. No historical data endpoint — same limitation as Nest; Trove must poll and persist.** · M5 · OAuth (Honeywell developer registration) · effort **M** · 🆕 new

- **Access:** REST at developer.honeywellhome.com — OAuth 2.0 with Consumer Key + Consumer Secret (register at developer.honeywellhome.com). Supports T-Series (T9, T10), Lyric Round, and other Honeywell Home Wi-Fi thermostats. Endpoints: GET /v2/devices/thermostats, GET /v2/devices/thermostats/{deviceId}.
- **Recommendation:** Build later — same pattern as Nest but for the large Honeywell installed base. Bring-your-own OAuth credentials; register once. Low ongoing cost.
- **Notes:** Physical device required; no simulator. API may return sensor data from Honeywell smart room sensors if present. No Ecobee fallback needed — this covers the main Honeywell/Resideo market. Third-party wrapper libraries in Go (gohoneywellapi) available.

#### Ambient Weather (personal weather station) — _Personal Weather Station_

🟢 **High — official documented REST API (ambientweather.docs.apiary.io), no OAuth friction, simple key-based auth. Data: outdoor temp/humidity/wind/rain/UV/solar, indoor temp/humidity, hourly/daily summaries.** · M5 · API key (bring-your-own from ambientweather.net account) · effort **S** · 🆕 new

- **Access:** REST at api.ambientweather.net/v1/devices — requires API Key + Application Key from ambientweather.net account. Returns real-time and historical observations (up to 1 year retained at 5-min resolution, then 30-min for older data). Data deleted after 1 year.
- **Recommendation:** Build now — simple API, personal data, weather enthusiast niche. Complement to WeatherFlow. 1-year retention means import-soon matters.
- **Notes:** API returns JSON. Bring-your-own API key + Application Key (both needed). Historical retention: 1 year at 5-min, older at 30-min, deleted after 1 year — Trove prevents data loss. Ambient Weather's station IDs are in the device list response.

#### Aranet4 CO2 / Air Quality (BLE local) — _Indoor Air Quality_

🟡 **Medium — BLE protocol is community-documented, Rust library exists. macOS pairing sometimes finicky on M-series Macs (known forum complaints). Sensor must be physically in Bluetooth range at collection time. No cloud dependency at all — fully local.** · M4 · TCC: Bluetooth (CoreBluetooth entitlement for Tauri app) · effort **M** · 🆕 new

- **Access:** Bluetooth Low Energy directly from sensor. Characteristic UUID f0cd3001-95da-4f4b-9ac8-aa55d312af0c for current readings; separate characteristic for historical log download. Rust crate available (github.com/cameronrye/aranet). Pairs with macOS Bluetooth once; sensor stays in range.
- **Recommendation:** Spike first — verify macOS BLE pairing reliability on recent hardware before committing. High value if it works (CO2, temperature, pressure, humidity, battery; historical download). Aranet4 is the gold standard CO2 monitor.
- **Notes:** Aranet4 stores up to ~14 days of readings on-device. BLE download retrieves full history. Rust crate is community-maintained. Alternative: Aranet cloud app export (CSV) as M1 fallback if BLE proves unreliable. macOS M-series BLE pairing issues reported but workarounds exist.

#### Awair Element (indoor air quality — local API + cloud) — _Indoor Air Quality_

🟢 **High — official Awair local API feature documented (support.getawair.com/hc/en-us/articles/360049221014). Rust crate exists (blog.yossarian.net/2023/03/20/Introducing-awair-local-api-rs). Data export to CSV also available from Awair dashboard.** · M5 · none for local LAN HTTP; OAuth for cloud API · effort **S** · 🆕 new

- **Access:** Local API: device hosts HTTP server on LAN at http://<device-ip>/air-data/latest — returns real-time JSON (CO2, VOC, PM2.5, temp, humidity, score). Enable in Awair Home app. Awair Omni/Enterprise have it on by default. Cloud: Awair Developer API at developer.getawair.com — OAuth, historical 5-min data.
- **Recommendation:** Build now — dual path: LAN poll for real-time air quality, CSV export as M1 fallback. Awair is a popular prosumer IAQ monitor. Local API = no account needed.
- **Notes:** Local API only returns current reading, no history. Cloud API provides historical data. Models: Awair 2nd Ed, Awair Element support local API (beta enable required). Awair dashboard CSV export covers any date range > 1 year via multiple exports. Cloud API OAuth is free for personal use. Airthings Wave Plus (competitor) also has a consumer cloud API at developer.airthings.com — same M5 pattern; consumer API requires Client ID + Secret from Airthings developer portal.

#### Airthings Wave / View (indoor air quality + radon) — _Indoor Air Quality_

🟢 **High — official documented consumer API, free registration, personal device data. Data: radon (Bq/m3), CO2, VOC, PM, temp, humidity, pressure. Dashboard CSV export available as M1 fallback.** · M5 · OAuth (Airthings developer portal — free account) · effort **S** · 🆕 new

- **Access:** Consumer cloud API at developer.airthings.com (documentation moved to consumer-api-doc.airthings.com). OAuth Client Credentials with Client ID + Secret from Airthings developer portal. Endpoints for latest samples and historical data per device. CSV export also available from Airthings dashboard.
- **Recommendation:** Build later — same pattern as Awair but cloud-only. Notable for radon data which is unique to Airthings. Pair with Awair for comprehensive IAQ coverage.
- **Notes:** Airthings business API (airthings.com/en/business/api) is separate from consumer. Consumer API confirmed as of 2025 via Universal Devices forum. Dashboard → select device → Export to CSV available. Historical data retention: unclear but > 1 year observed.

#### Sense Energy Monitor — _Home Energy Monitor_

🟡 **Medium — no official API (community has requested it for years with no response). Unofficial library works but could break on API changes. CSV export is stable but manual. Device-level usage detection (always-on, fridge, EV charger, etc.) is valuable.** · M5; M1 fallback · Account credentials (unofficial); CSV export = none · effort **M** · 🆕 new

- **Access:** No official API. Unofficial reverse-engineered API via python library (github.com/scottbonline/sense) — authenticates with Sense account credentials, returns real-time WebSocket stream and device-level usage. Web app has data export: Usage screen → export CSV for historical kWh data.
- **Recommendation:** Spike first — test unofficial API stability. Start with M1 CSV import as guaranteed fallback. If unofficial library holds, upgrade to M5 polling. Energy device detection data is unique.
- **Notes:** Sense identifies individual appliances by electrical signature — data includes device names, usage history, cost estimates. Real-time: WebSocket stream from unofficial API. Official export: hourly CSV from web app. No rate limits on unofficial API currently. iRobot/Roomba comparison: Sense is cloud-only, no local path.

#### Emporia Vue (home energy monitor) — _Home Energy Monitor_

🟡 **Medium — Emporia explicitly acknowledges pyemvue as community-supported but unsupported by them. All data flows through Emporia cloud. 5-second resolution available via unofficial API; 1-minute and hourly aggregates. Ported to Rust possible via HTTP client.** · M5 · Account credentials (unofficial) · effort **M** · 🆕 new

- **Access:** No official API. Unofficial access via pyemvue Python library (pypi.org/project/pyemvue) — authenticates with Emporia account, pulls per-circuit energy data from cloud. No local API (device has no local server). CSV export: none documented.
- **Recommendation:** Build later — good value for circuit-level energy data (up to 16/50 circuits). Unofficial API is actively maintained (vuegraf PyPI, used in production). Risk: Emporia could break it.
- **Notes:** Emporia Vue 3 (2026 model) supports 18 circuits. Long-term official API goal stated but no timeline. Data: per-circuit kWh at 1-second to daily resolution. Solar net metering supported. Community library vuegraf writes to InfluxDB — shows the data richness.

#### Tesla Powerwall + Solar (local gateway + Fleet API) — _Solar / Battery Storage_

🟡 **Medium — local API is unofficial/undocumented, affected by firmware updates (as of FW 25.10.0 TEDAPI LAN routing removed — must connect to Powerwall Wi-Fi AP directly). Fleet API is official but requires developer registration and Tesla account. pypowerwall (Python) and vloschiavo/powerwall2 community docs cover both paths.** · M5 · Account credentials (local) or OAuth (Fleet API — requires Tesla developer account) · effort **L** · 🆕 new

- **Access:** Local: HTTPS to Powerwall gateway at its LAN IP or 192.168.91.1 (Wi-Fi AP) — auth with Tesla email + last 5 digits of gateway serial. Cloud: Tesla Fleet API at developer.tesla.com — OAuth, /api/1/energy_sites/{id}/calendar_history for historical, /telemetry_history for wall connector.
- **Recommendation:** Build later — niche (Powerwall owners only) but high value for solar+battery data. Use Fleet API as primary (more stable), local as fallback. Track firmware changes.
- **Notes:** Fleet API pricing: tiered by request volume as of Jan 2025 — low-volume personal use may be free tier. calendar_history returns daily/weekly aggregates. Local API: Powerwall 1/2/+ more stable than PW3 (TEDAPI firmware changes). pypowerwall handles both paths. Wall Connector charging history via /telemetry_history endpoint.

#### Ring (doorbell / camera event logs) — _Security / Doorbell_

🟡 **Medium — unofficial API actively maintained as of Feb 2026 (v0.9.14 release). Provides event history, device health, alert metadata. Risk: Ring (Amazon) can change private API at any time. No official API and no export feature.** · M5 · Account credentials (unofficial) · effort **M** · 🆕 new

- **Access:** Unofficial reverse-engineered API only — no official public API. python-ring-doorbell (github.com/python-ring-doorbell, updated Feb 2026) or ring-client-api (npm). Auth: Ring account email + password + 2FA. Event history: motion detections, dings, on-demand recordings. Metadata only (timestamps, event type, device); video clips require separate download.
- **Recommendation:** Build later — event logs (who rang, motion timestamps, device) are high personal-context value. Unofficial but stable enough to build against. Note the standlone rule: this is a cloud HTTP call (not requiring Ring app running), so it passes.
- **Notes:** Event history retention: up to 180 days in the Ring cloud. Video thumbnail URLs available but require auth. ring-client-api (Node/TS) may be easier to wrap via Tauri sidecar. Python library had firebase-messaging migration in 2025. Amazon Alexa data (voice history export via amazon.com/privacy): M1 import of alexa/voice_history.json from Amazon data download — timestamps + command text, excludes audio. 24-72hr export turnaround.

#### Home Assistant (self-hosted hub) — _Smart Home Hub_

🟡 **Medium — API is well-documented and stable. CRITICAL STANDALONE CONSTRAINT: Home Assistant must be running for the API to work. Trove cannot require HA to be running. However: if a user self-hosts HA, treating it as an always-on data source that Trove polls opportunistically (not requiring it) is acceptable. Rich history data covering ALL home devices HA knows about.** · M5 · none (local HTTP; user must have HA running and provide token) · effort **M** · 🆕 new

- **Access:** REST API at http://homeassistant.local:8123/api — Long-Lived Access Token from HA profile page. Key endpoints: /api/history/period/{timestamp} (state history), /api/logbook/{timestamp} (activity log), /api/states (current states). WebSocket API preferred for new integrations.
- **Recommendation:** Build later — optional integration for users who already run HA. Frame as 'if you run Home Assistant, Trove can pull all your device history from it.' Do NOT make Trove depend on HA. Opportunistic poll: if HA unreachable, skip gracefully. Single integration aggregates Zigbee, Z-Wave, Nest, Hue, etc.
- **Notes:** HA history stores state changes for all entities — temperature, motion, locks, lights, switches, etc. Default retention: 10 days (configurable to longer). SQLite or PostgreSQL backend. WebSocket API for real-time. This is the single highest-leverage integration for users who have HA, but it requires HA to be running (standalone nuance). Treat as optional/opportunistic, never required.

#### Netatmo Personal Weather Station — _Personal Weather Station_

🟢 **High — official documented API, well-supported (pyatmo Python library, PHP SDK, Go CLI). Historical data queryable with date range. No data retention limit mentioned.** · M5 · OAuth (Netatmo developer app registration — free) · effort **S** · 🆕 new

- **Access:** REST API at dev.netatmo.com — OAuth 2.0, Create App at dev.netatmo.com → get client_id + client_secret. Key endpoint: /api/getmeasure — retrieves historical observations for any owned station. Data: outdoor/indoor temperature, humidity, CO2, noise, pressure, rain, wind.
- **Recommendation:** Build later — solid API for popular prosumer weather station. Rich multi-module data including indoor CO2 and noise.
- **Notes:** Netatmo Weather API docs at dev.netatmo.com/apidocumentation/weather. OAuth mandatory — user must consent. Historical /getmeasure endpoint accepts start/end timestamps, returns values at configured intervals. Modules: base station, outdoor, rain, wind, additional indoor. Also: Netatmo HOME Coach (indoor air quality) uses same API.

#### Moen Flo Smart Water Monitor — _Water Monitoring_

🟡 **Medium — integration is live in Home Assistant (maintained), unofficial but stable. Water usage data is unique personal data not available elsewhere. No local API; device is cloud-dependent.** · M5 · Account credentials (unofficial) · effort **M** · 🆕 new

- **Access:** Unofficial cloud API — reverse-engineered by Home Assistant Flo integration (home-assistant.io/integrations/flo). Auth with Flo account credentials. Returns: flow rate (gal/min), water temperature, pressure, daily/weekly/monthly consumption, leak detection events, valve state.
- **Recommendation:** Build later — water consumption history is compelling data for sustainability tracking. Unofficial but HA integration provides working reference implementation.
- **Notes:** Flo integration in HA returns flow_today, consumption_today_gallons. Phyn Plus (competitor) — per January 2025 reports, no API access planned, no data export. Skip Phyn. Green Button (utility meters) is a better path for total household water if utility supports it.

#### Utility Smart Meter (Green Button) — _Energy / Utility_

🟡 **Medium — Green Button DMD (download) is supported by most major US utilities. CMD (API) is rarely supported and requires utility-specific registration. XML/ESPI format needs parsing. Data: hourly or 15-min electricity usage (kWh), sometimes gas.** · M1 · none (manual download from utility account) · effort **M** · 🆕 new

- **Access:** Green Button Download My Data: utility website → account → energy usage → 'Download My Data' button → XML file (ESPI format). Green Button Connect My Data (CMD): automated API — fewer than 55 utilities support it (adoption is sparse). Utility-specific portals: PG&E, ConEd, ComEd, etc. each have their own export flow.
- **Recommendation:** Build now (M1 importer) — manual Green Button XML import covers the vast majority of US users. Parse ESPI XML into daily/hourly energy JSONL. Add utility-specific CSV parsers for utilities that don't support Green Button.
- **Notes:** ESPI (Energy Services Provider Interface) XML schema is an open standard — parseable with quick_xml in Rust. File typically named GreenButton_*.xml or usage.xml. Some utilities export CSV instead (PG&E exports both). Gas meters often included. Water meters: rare but some utilities bundle all utilities in one export. UtilityAPI.com is a third-party aggregator — overkill for personal use.

#### Roborock Robot Vacuum (cleaning history + maps) — _Robot Vacuum_

🟡 **Medium — local protocol is community-documented via python-miio (widely used). Cleaning history includes: start time, duration, area cleaned, error code, map snapshot. Roborock's official API is closed; iRobot filed Chapter 11 bankruptcy Dec 2025 (brand uncertain).** · M5 · none for local LAN (if token obtained); cloud auth needed to retrieve token · effort **M** · 🆕 new

- **Access:** Via python-miio library — local protocol (MIIO protocol on UDP port 54321, LAN IP). Auth: device token (extractable from Roborock app via Xiaomi cloud auth or third-party tools). Methods: clean_history(), get_maps(), last_clean_details(). Home Assistant Roborock integration uses cloud pairing then local comms.
- **Recommendation:** Build later — cleaning history provides daily activity pattern context (when cleaning happened, area covered). Use python-miio protocol documented behavior; wrap as Rust async UDP client.
- **Notes:** Token extraction requires one-time Xiaomi cloud login (or manual extraction from Android app). Once token is in vault, local-only forever. Maps are binary-encoded images (proprietary format). iRobot Roomba: also has unofficial API but brand future uncertain post-bankruptcy. Roborock is safer bet for 2026+.

#### August / Yale Smart Lock (entry logs) — _Smart Lock_

🟡 **Medium — yalexs is the reference implementation used by Home Assistant. Undocumented API can change. Entry log is valuable security + presence data. No local API (requires Yale cloud).** · M5 · Account credentials (unofficial) · effort **M** · 🆕 new

- **Access:** Unofficial cloud API at developer.august.com (undocumented, reverse-engineered). Python library: yalexs (github.com/Yale-Libs/yalexs). Auth: August/Yale account + phone number (SMS 2FA). Returns: lock/unlock event log with user, method (app, keypad, auto-lock), timestamp.
- **Recommendation:** Build later — entry/exit log with who unlocked is high personal-context value. Unofficial but actively maintained in yalexs. Known instability: event listener breaks after 1-2 days (HA bug reports), needs periodic reconnect.
- **Notes:** yalexs returns lock activity log including guest codes. Schlage Encode: similar unofficial API. Kwikset Halo: SmartThings API path. All smart locks use cloud backends — no local protocol option for any major consumer brand. Pick August/Yale as largest installed base. Two-factor via SMS required at auth time.

#### TP-Link Kasa Smart Plugs (energy monitoring) — _Smart Plugs / Energy_

🟡 **Medium — local protocol documented and stable for older models (HS110, KP115). Newer Tapo-branded devices (TP100, P110) use a different encrypted protocol; python-kasa now supports both Kasa and Tapo. Some firmware versions have removed local API — check model-specific status.** · M5 · none (LAN TCP/UDP) · effort **S** · 🆕 new

- **Access:** python-kasa library (github.com/python-kasa/python-kasa) — LAN UDP/TCP protocol; no account needed if on same network. Auth: device IP. Energy monitoring models (KP125, HS110, EP25): return emeter data — current, voltage, power (W), daily/monthly kWh totals. Cloud API also available via tplink-cloud-api.
- **Recommendation:** Build later — per-plug energy monitoring complements Sense/Emporia for granular appliance tracking. Local LAN protocol = no cloud dependency once devices are on network.
- **Notes:** python-kasa handles discovery (broadcast) and individual device polling. Energy data: daily kWh, current W, monthly totals. Models with energy monitoring: HS110, KP115, KP125, EP25 (Kasa); P110, P115, EP25 (Tapo). Tapo protocol requires one-time cloud auth to get local credentials. Rust port: wrap python-kasa via sidecar or reimplement Kasa JSON protocol (documented by community).

#### SwitchBot (hub + sensors) — _Smart Home / Sensors_

🟢 **High — official published API with HMAC auth, well-documented (github.com/OpenWonderLabs/SwitchBotAPI). BLE devices require SwitchBot Hub to bridge to cloud. Temperature, humidity, motion, contact events all queryable.** · M5 · API key (bring-your-own from SwitchBot app) · effort **S** · 🆕 new

- **Access:** Official open API v1.1 at api.switch-bot.com — HMAC-SHA256 token + secret from SwitchBot app → Profile → Developer Options. Rate limit: 10,000 req/day. Devices: Hub 2 (temp/humidity display), Meter Plus, outdoor meter, motion sensor, contact sensor, curtains, plugs. Returns device states and scenes.
- **Recommendation:** Build later — straightforward official API. SwitchBot is popular affordable sensor ecosystem. Hub 2 built-in temperature/humidity makes it a common IAQ sensor.
- **Notes:** Webhook support available (v1.1) for push events. BLE-only devices (Bot, Curtain) need Hub for cloud visibility. Local LAN API exists via SwitchBot Hub 2 (unofficial, documented by community) but cloud API is simpler. Scene execution also available.

#### Ecobee Smart Thermostat — _Smart Thermostat_

🟠 **Low — new developer accounts not being accepted as of June 2026 with no stated timeline for reopening. Existing integrations work but Trove cannot onboard new users with this path. HomeKit fallback only provides config data, not telemetry.** · M5 · OAuth (blocked for new registrations) · effort **L** · 🆕 new

- **Access:** Developer API at ecobee.com/en-us/developers — OAuth 2.0. BLOCKED: Ecobee stopped accepting new developer registrations as of April 2024; no new API keys being issued as of October 2024. Existing keys continue to work. Workaround for HomeKit-capable Ecobee models: use HomeKit/homed DB (M3) to read device config.
- **Recommendation:** Icebox — blocked on new developer registration. Revisit if Ecobee reopens. Note in UI: 'Ecobee integration temporarily unavailable — developer registrations paused.' Ecobee 3 and later support HomeKit; homed DB captures device presence.
- **Notes:** Ecobee premium features (energy reports, runtime history) are cloud-only. Home Assistant community workaround: use HomeKit Device integration locally — captures current state but no history. If Ecobee reopens developer registration, this becomes a Medium-effort M5 with rich historical runtime data (heat/cool runtime, setpoints, occupancy sensor history).

#### Amazon Alexa (voice history export) — _Voice Assistant_

🟡 **Medium — export works but is manual and asynchronous (24-72hr). Audio recordings excluded. As of March 2025, Amazon disabled 'Do Not Send Voice Recordings' — all Alexa+ interactions go to cloud. Smart home device event logs NOT included in export. Only voice command transcriptions.** · M1 · none (Amazon account login) · effort **S** · 🆕 new

- **Access:** Amazon data download portal at amazon.com/privacy → Request My Data → 'Alexa interaction history'. Export arrives via email link in 24-72 hours. File: alexa/voice_history.json — timestamps, device names, command text, Alexa response text. Format: JSON (recommend over CSV for metadata fidelity).
- **Recommendation:** Build now (importer only) — simple JSON import, timestamps + commands create a useful voice-query activity log. No API available for live pull.
- **Notes:** Export covers up to 18 months; older data may be gone. alexa/smart_home_history.json may be included — check Alexa privacy portal for current available categories. No API for programmatic ongoing pull — export is the only path. Alexa Skills API is for skill developers, not personal data retrieval.

#### Lutron Caséta (smart lighting / shades) — _Smart Lighting_

🟡 **Medium — LEAP protocol local access works well on Smart Bridge PRO but requires the PRO model ($70 more than standard bridge). No history stored in bridge — only current state; Trove must poll and accumulate. pylutron-caseta is actively maintained.** · M5 · none (LAN TCP — PRO bridge required for local access) · effort **M** · 🆕 new

- **Access:** Caséta Smart Bridge PRO: local LEAP protocol over LAN (port 8081/8083). Python library: pylutron-caseta (pypi). Also supports telnet on port 23 with Lutron Integration Protocol (older). Standard Bridge (non-PRO): cloud API only via Lutron app; local LEAP NOT supported on standard bridge.
- **Recommendation:** Build later — popular in US homes, fully local for PRO bridge owners. Captures lighting state + occupancy sensor events. Frame as PRO-bridge-required in UI.
- **Notes:** LEAP protocol gives device list, current level for all lights/fans/shades, occupancy sensor state. Pico remote events also visible. No historical data on bridge. telnet/LIP protocol works on older systems. Non-PRO bridge users: cloud-only path via Lutron's cloud connector (undocumented, use Home Assistant as intermediary if HA is available).

### Home, IoT & Smart Devices — cross-cutting notes

1. LOCAL-vs-CLOUD SPLIT: Every source in this domain falls into one of three buckets: (a) fully local LAN/BLE (Hue, Tempest UDP, Enphase gateway, Aranet4, Awair local, Kasa, Lutron LEAP) — these are the most robust and privacy-respecting; (b) cloud API pull with user OAuth (Nest SDM, Honeywell, Ambient Weather, Airthings, SwitchBot, Ring) — requires ongoing token management; (c) manual export/import (Green Button, Alexa, Awair CSV) — reliable but asynchronous. Trove should clearly communicate to users which tier each source uses.

2. HISTORY vs. CURRENT-STATE: The most common gap across this entire domain is that device APIs return CURRENT state only (Nest SDM, Hue, Kasa, Lutron). Trove must implement its own polling-and-accumulation loop — write a timestamped JSONL row on each poll — to build personal history. Design a shared 'poll-and-append' scheduler in troved that any IoT collector can plug into. Default poll interval: 5 minutes for thermostats, 1 minute for energy monitors.

3. HOME ASSISTANT AS SUPERCONNECTOR: For users who already run Home Assistant, a single HA REST API integration can pull history for ALL their home devices (Zigbee, Z-Wave, Nest, Hue, Ecobee via HomeKit path, etc.) in one shot. This should be a special 'aggregate' integration clearly labeled 'requires Home Assistant running.' It does not violate standalone rules if framed as optional/opportunistic.

4. ENERGY VAULT SCHEMA: Solar (Enphase/Tesla), utility (Green Button), monitor (Sense/Emporia/Kasa plugs), EV charger (Tesla Wall Connector via Fleet API) all share a common schema: {timestamp, source, direction (import/export/generation/consumption), watts_or_kwh, interval_minutes}. A unified energy JSONL format works across all five sources. Map immediately to 'energy/' vault subfolder.

5. BLOCKED/FRAGILE SOURCES: Ecobee (registration closed), Alexa (export only, no live API), Tesla local gateway (firmware-breakable), Ring (unofficial only). These need graceful degradation UI — show as 'connect' with a note about current status. Never silently fail.

6. ARANET4 / BLE: The Bluetooth/CoreBluetooth permission is the only new TCC permission this domain adds. A single Tauri BLE entitlement could also cover SwitchBot BLE-direct (Hub-less Bot/Curtain devices). Worth adding the entitlement to the Tauri config early even if only Aranet4 uses it initially.

7. ECOBEE WORKAROUND: Ecobee 3+ models support HomeKit — the homed M3 DB read (already recommended) captures device presence and config. A note in the Ecobee integration card pointing to this path avoids the user seeing a hard block.

8. NETATMO / AMBIENT WEATHER / TEMPEST OVERLAP: All three are personal weather stations. Design a unified 'personal weather station' schema ({timestamp, temp_c, humidity_pct, pressure_hpa, wind_mps, rain_mm, co2_ppm, ...}) and map all three into it. Open-Meteo (already built) provides the regional context; personal stations provide hyper-local ground truth.

---

## Environment & Ambient Context

This domain covers the physical world surrounding the user: weather conditions, air quality, daylight cycles, astronomical events, geological activity, marine/hydrological conditions, and ambient sound. The foundational Open-Meteo weather collector is already built in Trove (hourly conditions, UV index, CoreLocation bridge, Weather tab). The domain is exceptionally well-served by keyless public APIs — NOAA, USGS, Open-Meteo, and USNO collectively cover weather, air quality, earthquakes, tides, aurora, river flood, marine, sunrise/sunset, and moon phases with zero registration friction. Historical backfill is broadly available (Open-Meteo back to 1940, USGS earthquake catalog open-ended, NOAA tides multi-decade) making this one of the safest domains to build incrementally since data is always re-fetchable. The one non-trivial friction point is pollen/allergy data for the US, where the best keyless option (Open-Meteo CAMS) is Europe-only at high resolution and global at coarser granularity.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Open-Meteo Weather Forecast + Historical Archive | Weather | M5 | none (location from CoreLocation TCC or manual config) | S | 🟢 High — already built. Zero auth friction, 10k calls/day free, historical archive back to 1940 for free backfill. UV index available as hourly variable. | ✅ built |
| Open-Meteo Air Quality API | Air Quality | M5 | none | S | 🟢 High — same base URL pattern as existing weather collector, zero additional auth. Pollen is Europe-only at high resolution (global coverage drops pollen). | 📋 planned |
| USNO Astronomical Applications API (sunrise, sunset, moon phases, twilight) | Daylight & Astronomy | M5 | none | S | 🟢 High — no auth, covers 1700–2100, stable US Navy service. | 🆕 new |
| USGS Earthquake Catalog | Geophysical | M5 | none | S | 🟢 High — fully keyless, stable FDSN-standard API, unlimited historical backfill, global coverage. | 🆕 new |
| NOAA CO-OPS Tides & Currents API | Marine / Tides | M5 | none | S | 🟢 High — keyless, US-focused (3,000+ stations), high/low predictions up to 10 years out, observations 45-day rolling. Coastal users only; not useful for inland locations. | 🆕 new |
| Open-Meteo Marine Weather API | Marine | M5 | none | S | 🟢 High — keyless, global ocean coverage. Returns null/empty for inland lat/lon so safe to call unconditionally. | 🆕 new |
| NOAA SWPC Space Weather / Aurora (Kp index, geomagnetic alerts) | Space Weather | M5 | none | S | 🟢 High — fully keyless US government service, updated in near-real-time, stable. | 🆕 new |
| NWS api.weather.gov (US weather alerts + hourly forecast) | Weather Alerts | M5 | none | S | 🟢 High — keyless, stable, official US government NWS API. US coverage only; non-US users fall back to Open-Meteo weather codes. | 🆕 new |
| Open-Meteo Flood API (GloFAS river discharge) | Hydrological | M5 | none | S | 🟢 High — keyless, global, based on Copernicus Emergency Management Service GloFAS v4. | 🆕 new |
| NASA FIRMS (wildfire / active fire detection) | Wildfire | M5 | API key (free, email signup) | M | 🟢 High — free key with generous limits, reliable NASA/LANCE service. Requires one-time email signup for map key. | 🆕 new |
| USGS Water Services (stream gauges / flood stage) | Hydrological | M5 | none | M | 🟢 High — keyless, US-only, covers ~10,000 active stream gauges with real-time gage height and flood stage thresholds. | 🆕 new |
| AirNow API (US EPA AQI) | Air Quality | M5 | API key (free, email registration) | S | 🟢 High — free key, EPA-authoritative US AQI data, covers PM2.5, PM10, ozone, CO, NO2, SO2. Better official source for US AQI than Open-Meteo CAMS for US users. | 🆕 new |
| WAQI / aqicn.org (World Air Quality Index) | Air Quality | M5 | API key (free, instant token via web form) | S | 🟢 High — free token with very generous quota, global coverage from 10,000+ monitoring stations, single endpoint for worldwide AQI. | 🆕 new |
| Open-Meteo (UV index, already in weather stream) | UV Index | M5 | none | S | 🟢 High — already collected. | ✅ built |
| sunrise-sunset.org / SunriseSunset.io (golden hour, twilight stages) | Daylight & Astronomy | M5 | none | S | 🟢 High — fully keyless, simple REST, comprehensive twilight data including golden hour. Alternative to USNO for purely sunrise/sunset use cases. | 🆕 new |
| USNO Moon Phases API | Daylight & Astronomy | M5 | none | S | 🟢 High — same USNO service as sunrise/sunset (already listed), trivial to add moon phase pull in the same daily astronomy collector. | 🆕 new |
| NOAA National Data Buoy Center (NDBC) — ocean/marine observations | Marine | M5 | none | M | 🟡 Medium — keyless, but data is plain-text tabular files (not JSON); requires custom parser. US coastal and Great Lakes coverage; ~1,000 active stations. | 🆕 new |
| Apple Health export.zip — Environmental Audio Exposure | Ambient Sound | M1 | none (user-initiated export from iPhone) | S | 🟢 High — data is already in the existing Apple Health export.zip import path. Requires adding two new metric types to the health importer. | 🆕 new |
| macOS Microphone — Live Ambient dB Level | Ambient Sound | M4 | TCC Microphone (Privacy → Microphone in System Settings) | M | 🟡 Medium — API exists and works on macOS. Important caveat: consumer laptop microphones have inconsistent frequency response and pre-amp gain, so absolute dB SPL values are unreliable for comparison across devices. Values are useful for relative trend tracking (quiet vs. loud environment) but not calibrated. | 🆕 new |
| NOAA Climate Data Online (CDO) — Historical Weather Observations | Weather | M5 | API key (free, instant email registration) | M | 🟡 Medium — useful for one-time historical backfill of observed (vs. reanalysis) weather data from actual nearby NOAA stations. Open-Meteo ERA5 archive (already planned) covers same use case with zero auth; CDO adds station-specific ground truth. | 🆕 new |
| PurpleAir API (Hyperlocal PM2.5 from community sensors) | Air Quality | M5 | API key (free, Google SSO signup) | M | 🟡 Medium — free key, excellent data density in metro areas, 2-minute update rate. Sensor density is uneven (dense in wealthy US/EU cities, sparse elsewhere). | 🆕 new |
| Google Maps Pollen API (US/global pollen forecast) | Pollen / Allergy | M5 | API key (Google Cloud billing required, free tier: 5,000/month per SKU) | M | 🟡 Medium — best US pollen forecast coverage and global reach (65+ countries), but requires Google billing account setup. Free tier is 5,000 calls/month (sufficient at 1 call/day = ~365/year). | 🆕 new |
| Blitzortung / LightningMaps (lightning strike detection) | Lightning | M5 | none (MQTT public broker; HTTP endpoint public) | L | 🟠 Low — usage policy requires third-party apps to serve data from their own servers rather than direct client-to-broker connections; not suitable for a local-first app where each instance hits the broker directly. Commercial alternatives (OpenWeather Lightning, Xweather, Vaisala) require paid keys. | 🆕 new |
| macOS CoreMotion barometric pressure (CMAltimeter) | Weather | M4 | N/A | XL | 🔴 Blocked — CMAltimeter is not available on macOS regardless of hardware. Macs with Apple Silicon do contain barometer hardware but Apple does not expose it via public APIs on macOS. Barometric pressure is available from Open-Meteo forecast API (pressure_msl field already in the weather struct). | 🆕 new |
| Open-Meteo Climate Change API (CMIP6 projections) | Climate | M5 | none | M | 🟡 Medium — keyless and technically feasible, but more useful as a one-time analytical query than a recurring collection target. Projections (future) and bias-corrected historical (past) rather than observed conditions. | 🆕 new |

### Detail

#### Open-Meteo Weather Forecast + Historical Archive — _Weather_

🟢 **High — already built. Zero auth friction, 10k calls/day free, historical archive back to 1940 for free backfill. UV index available as hourly variable.** · M5 · none (location from CoreLocation TCC or manual config) · effort **S** · ✅ built

- **Access:** https://api.open-meteo.com/v1/forecast (7–16 day forecast, daily sunrise/sunset/UV), https://archive-api.open-meteo.com/v1/archive (ERA5 back to 1940, ERA5-Land back to 1950). Parameters: latitude, longitude, hourly/daily variable lists, timezone. No key.
- **Recommendation:** Build now — deepen with daily sunrise/sunset/golden-hour variables already available in this same endpoint (see below); they should be added to the existing WeatherObservation struct.
- **Notes:** Already collects hourly: temp, apparent temp, humidity, dew point, precip, snow, weather code, cloud cover, pressure, wind, UV index, is_day. Daily variables also available in same call: sunrise, sunset, precipitation_sum, wind_speed_max. Historical archive available at archive-api.open-meteo.com/v1/archive; weather is always re-fetchable so backfill is safe to run lazily.

#### Open-Meteo Air Quality API — _Air Quality_

🟢 **High — same base URL pattern as existing weather collector, zero additional auth. Pollen is Europe-only at high resolution (global coverage drops pollen).** · M5 · none · effort **S** · 📋 planned

- **Access:** https://air-quality-api.open-meteo.com/v1/air-quality — hourly 5-day forecast. Variables: pm10, pm2_5, carbon_monoxide, nitrogen_dioxide, sulphur_dioxide, ozone, aerosol_optical_depth, dust, ammonia, methane. UV index and UV index clear sky. European AQI and US AQI (consolidated + per-pollutant). Pollen: alder, birch, grass, mugwort, olive, ragweed (Europe-only, 11 km; global at 45 km for non-pollen variables). No key.
- **Recommendation:** Build now — trivial extension of existing M5 HTTP client; add a separate hourly poll (~every 3 hours is sufficient given 5-day forecast window) and write to weather/air-quality/YYYY-MM.jsonl.
- **Notes:** Data source is Copernicus CAMS (European Centre for Medium-Range Weather Forecasts). Global PM2.5/PM10/ozone/NO2 at 45 km resolution; Europe also gets pollen and 11 km resolution. For non-European users, pollen fields will be absent — handle gracefully. Historical air quality is not available via this endpoint (forecast only); for historical AQ see NOAA CDO or OpenWeather (key required).

#### USNO Astronomical Applications API (sunrise, sunset, moon phases, twilight) — _Daylight & Astronomy_

🟢 **High — no auth, covers 1700–2100, stable US Navy service.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://aa.usno.navy.mil/api/rstt/oneday?date=YYYY-MM-DD&coords=LAT,LON&tz=OFFSET&dst=true — returns sunrise, sunset, solar noon, civil/nautical/astronomical twilight begin/end, moonrise, moonset, moon phase. https://aa.usno.navy.mil/api/moon/phases/year?year=YYYY — full year of primary lunar phases. Truly keyless (optional 8-char ID for tracking only). Returns JSON.
- **Recommendation:** Build now — one daily API call per user location gives sunrise, sunset, golden hour boundaries, all twilight stages, moon phase, and illumination. Store in weather/ alongside conditions. Alternatively, derive from Open-Meteo's daily sunrise/sunset variables (already in existing endpoint) and layer astronomical twilight from USNO.
- **Notes:** Golden hour is not a named field but is computable as ±1 hour around sunrise/sunset from the rstt/oneday response. Alternatively, SunriseSunset.io (sunrisesunset.io/api) provides golden_hour field directly, also keyless. USNO is authoritative and US-government-maintained. Data valid for any coordinate globally. Moon illumination fraction also available via separate /api/moon/illumination endpoint.

#### USGS Earthquake Catalog — _Geophysical_

🟢 **High — fully keyless, stable FDSN-standard API, unlimited historical backfill, global coverage.** · M5 · none · effort **S** · 🆕 new

- **Access:** Real-time feeds (no key): https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/all_hour.geojson (updated every minute), /all_day.geojson, /significant_week.geojson. Historical/custom: https://earthquake.usgs.gov/fdsnws/event/1/query?format=geojson&starttime=YYYY-MM-DD&endtime=YYYY-MM-DD&minmagnitude=2.5&latitude=LAT&longitude=LON&maxradiuskm=500 — paginated, 20k events max per request, offset supported.
- **Recommendation:** Build now — poll daily or hourly for a radius around the user's location, write to environment/earthquakes/YYYY-MM.jsonl. Backfill available for years of history on first run.
- **Notes:** Single response capped at 20,000 events; paginate with limit+offset for large historical queries. Good complement to the existing weather store: both are 'world around the user' streams. Magnitude threshold 2.5+ for local radius keeps noise low. The real-time GeoJSON feeds are preferred by USGS for performance.

#### NOAA CO-OPS Tides & Currents API — _Marine / Tides_

🟢 **High — keyless, US-focused (3,000+ stations), high/low predictions up to 10 years out, observations 45-day rolling. Coastal users only; not useful for inland locations.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://api.tidesandcurrents.noaa.gov/api/prod/datagetter?product=predictions&datum=MLLW&station=STATIONID&begin_date=YYYYMMDD&end_date=YYYYMMDD&interval=hilo&time_zone=lst_ldt&units=english&format=json — no key required. Station list: https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi/stations.json. Products: water_level (observations), predictions (tide tables), wind, air_pressure, water_temperature.
- **Recommendation:** Build now for coastal users — detect nearest tidal station from user's coordinates via the metadata API, skip silently if >50 km inland. Store daily high/low predictions in environment/tides/YYYY-MM.jsonl.
- **Notes:** Only covers US and territories. For global tides, Open-Meteo does not have a tides endpoint. WorldTides API (worldtides.info) covers global but requires a paid key. NOAA CO-OPS is sufficient for a US-first rollout; note gracefully for non-coastal and non-US users.

#### Open-Meteo Marine Weather API — _Marine_

🟢 **High — keyless, global ocean coverage. Returns null/empty for inland lat/lon so safe to call unconditionally.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://marine-api.open-meteo.com/v1/marine?latitude=LAT&longitude=LON&hourly=wave_height,wave_direction,wave_period,swell_wave_height,swell_wave_direction,wind_wave_height,ocean_current_velocity — 7-day hourly forecast. Daily aggregates also available (wave_height_max, etc.). No key.
- **Recommendation:** Build later — useful for coastal/boating users. Low complexity given same HTTP client pattern, but narrow audience compared to air quality or earthquake data.
- **Notes:** Data from ERA5 ocean wave model (ECMWF). Updated every 6 hours. Does not cover most inland freshwater bodies — only open ocean and coastal areas. Combine with NOAA CO-OPS for tide height at specific stations.

#### NOAA SWPC Space Weather / Aurora (Kp index, geomagnetic alerts) — _Space Weather_

🟢 **High — fully keyless US government service, updated in near-real-time, stable.** · M5 · none · effort **S** · 🆕 new

- **Access:** Keyless JSON endpoints at services.swpc.noaa.gov/json/: ovation_aurora_latest.json (aurora footprint map + intensity, updated ~5 min), planetary_k_index_1m.json (30-day Kp history, 3-hour intervals), 45-day-forecast.json, solar_probabilities.json. Geomagnetic storm alerts: services.swpc.noaa.gov/products/alerts.json or text/products/alerts.txt. Full directory browsable at https://services.swpc.noaa.gov/json/.
- **Recommendation:** Build now — Kp index is the key variable for aurora visibility. Poll every 3 hours (matching the Kp update cadence), write to environment/space-weather/YYYY-MM.jsonl. Include current Kp, 3-day forecast, and any active geomagnetic storm watches/warnings.
- **Notes:** The OVATION aurora JSON gives a latitude × longitude grid of aurora probability — useful for 'can I see aurora tonight?' notifications. Kp >= 5 = visible at mid-latitudes. For detailed historical archiving of space weather, NOAA NCEI maintains archives at ngdc.noaa.gov. Email subscription notifications also available from SWPC for push-style alerts, but the JSON polling is simpler for Trove.

#### NWS api.weather.gov (US weather alerts + hourly forecast) — _Weather Alerts_

🟢 **High — keyless, stable, official US government NWS API. US coverage only; non-US users fall back to Open-Meteo weather codes.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://api.weather.gov/alerts/active?point=LAT,LON — active NWS watches/warnings/advisories. https://api.weather.gov/points/LAT,LON — returns gridpoint for forecast URL. https://api.weather.gov/gridpoints/{office}/{x},{y}/forecast/hourly — 7-day hourly. All JSON/GeoJSON, no key. US-only.
- **Recommendation:** Build now as a thin alert layer — poll /alerts/active?point= every 30 minutes (NWS rate-limit floor), write active alerts to environment/alerts/current.jsonl, append to environment/alerts/YYYY-MM.jsonl. Surface severe alerts prominently in UI.
- **Notes:** Covers tornadoes, floods, severe thunderstorms, winter storms, fire weather, and 100+ other event types. CAP XML format also available. OpenAPI 3.0 spec at api.weather.gov/openapi.json. Last 7 days of historical alerts available via /alerts endpoint. US and territories only — for global alerts there is no free equivalent.

#### Open-Meteo Flood API (GloFAS river discharge) — _Hydrological_

🟢 **High — keyless, global, based on Copernicus Emergency Management Service GloFAS v4.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://flood-api.open-meteo.com/v1/flood?latitude=LAT&longitude=LON&daily=river_discharge — returns daily river discharge (m³/s) for nearest large river within 5 km. Variables: river_discharge, river_discharge_mean/median/max/min/p25/p75. Forecast 210 days ahead; historical back to 1984 (GloFAS reanalysis). No key.
- **Recommendation:** Build later — useful for users near rivers; harmless to poll for all users (returns null if no river within 5 km). Store in environment/flood/YYYY-MM.jsonl.
- **Notes:** Data resolution ~0.05 degrees (~5 km). Returns null for locations with no significant river nearby. Does not represent small urban waterways. Historical backfill available from 1984 via same endpoint with date range params.

#### NASA FIRMS (wildfire / active fire detection) — _Wildfire_

🟢 **High — free key with generous limits, reliable NASA/LANCE service. Requires one-time email signup for map key.** · M5 · API key (free, email signup) · effort **M** · 🆕 new

- **Access:** https://firms.modaps.eosdis.nasa.gov/api/area/csv/MAP_KEY/VIIRS_SNPP_NRT/BBOX/DAY_RANGE — returns CSV of active fire detections. Also country endpoint, KML footprints. Free MAP_KEY via email signup at firms.modaps.eosdis.nasa.gov/api/map_key/. Rate limit: 5,000 transactions / 10-minute interval. Global data within 3 hours of satellite overpass; US/Canada near-real-time.
- **Recommendation:** Build now — particularly high value in fire-prone regions (western US, Australia, etc.). Query a bounding box around user location daily. Store detections in environment/wildfire/YYYY-MM.jsonl. Key is free and easy to obtain.
- **Notes:** Two sensor sources available: MODIS (1 km resolution) and VIIRS_SNPP_NRT (375 m resolution, preferred for recent). Detections within 3 hours of satellite pass globally. Historical archive available via the same API with a date parameter. Note: wildfire smoke/AOD (aerosol optical depth) is already captured via the Open-Meteo Air Quality API without a separate key.

#### USGS Water Services (stream gauges / flood stage) — _Hydrological_

🟢 **High — keyless, US-only, covers ~10,000 active stream gauges with real-time gage height and flood stage thresholds.** · M5 · none · effort **M** · 🆕 new

- **Access:** https://api.waterdata.usgs.gov/observations/current?monitoring-location-id=SITEID&parameterCode=00065&format=json (gage height, ft) or parameterCode=00060 (discharge, cfs). Station discovery: https://api.waterdata.usgs.gov/monitoring-locations/?stateCd=CA&siteType=ST&format=json. No key. Updated APIs released 2025 via api.waterdata.usgs.gov; legacy at waterservices.usgs.gov/nwis/iv still active.
- **Recommendation:** Build later — more granular than the Open-Meteo GloFAS flood API for US users; returns actual flood stage categories (action/flood/moderate/major). Pair with USGS earthquake and NWS alerts for a full US ambient hazard monitor.
- **Notes:** US-only. The modernized api.waterdata.usgs.gov is preferred over legacy waterservices.usgs.gov (both still live as of mid-2026). Real-Time Flood Impact API (RTFI) at same base URL gives infrastructure vulnerability crosswalk. Real-time data refreshes every 15–60 minutes depending on station.

#### AirNow API (US EPA AQI) — _Air Quality_

🟢 **High — free key, EPA-authoritative US AQI data, covers PM2.5, PM10, ozone, CO, NO2, SO2. Better official source for US AQI than Open-Meteo CAMS for US users.** · M5 · API key (free, email registration) · effort **S** · 🆕 new

- **Access:** https://www.airnowapi.org/aq/observation/latLong/current/?latitude=LAT&longitude=LON&distance=25&format=application/json&API_KEY=KEY — free key via email registration at docs.airnowapi.org. Endpoint returns current AQI by pollutant for nearest reporting area. Also: forecast endpoint, historical observations, fire/smoke conditions.
- **Recommendation:** Build now as a US-specific complement to Open-Meteo air quality. AirNow gives official EPA AQI values from actual ground monitors vs. model estimates. Store AQI category + pollutant in environment/air-quality/ alongside model data.
- **Notes:** US and Canada only. Data comes from actual ground-level monitors (more accurate than satellite-derived model data for local conditions). Fire and smoke layer available (AirNow Fire and Smoke Map data). Free tier has no stated rate limit cap. Key is issued instantly by email. For non-US users, fall back to Open-Meteo air quality which is global.

#### WAQI / aqicn.org (World Air Quality Index) — _Air Quality_

🟢 **High — free token with very generous quota, global coverage from 10,000+ monitoring stations, single endpoint for worldwide AQI.** · M5 · API key (free, instant token via web form) · effort **S** · 🆕 new

- **Access:** https://api.waqi.info/feed/geo:LAT;LON/?token=TOKEN — returns AQI, dominant pollutant, station name. Token via https://aqicn.org/data-platform/token/ (free, instant, no billing required). Default quota: 1,000 req/sec.
- **Recommendation:** Build now as the global ground-monitor AQI complement to AirNow (US) and Open-Meteo (model). Covers cities in Asia, Europe, and elsewhere where AirNow has no data.
- **Notes:** Uses WHO/EPA AQI scale. Covers China, India, Europe, Southeast Asia well. Token is free and public — no email confirmation, just a web form. Some stations have gaps; fallback gracefully to Open-Meteo model data when station data is stale (> 2 hours). Unique in covering Asian urban air quality that neither AirNow nor CAMS adequately serves.

#### Open-Meteo (UV index, already in weather stream) — _UV Index_

🟢 **High — already collected.** · M5 · none · effort **S** · ✅ built

- **Access:** Already captured as uv_index field in the existing weather collector via the /v1/forecast hourly endpoint (same Open-Meteo call). No additional endpoint needed.
- **Recommendation:** Build now — already in the struct; ensure it is surfaced in the Weather tab UI with sunburn risk categories (0–2 low, 3–5 moderate, 6–7 high, 8–10 very high, 11+ extreme).
- **Notes:** uv_index field is already in WeatherObservation struct. UV index clear sky (uv_index_clear_sky) also available as a separate hourly variable to show potential UV under cloud-free conditions — worthwhile addition.

#### sunrise-sunset.org / SunriseSunset.io (golden hour, twilight stages) — _Daylight & Astronomy_

🟢 **High — fully keyless, simple REST, comprehensive twilight data including golden hour. Alternative to USNO for purely sunrise/sunset use cases.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://api.sunrise-sunset.org/json?lat=LAT&lng=LNG&date=YYYY-MM-DD&formatted=0 — returns sunrise, sunset, solar_noon, civil/nautical/astronomical twilight begin/end, day_length. Keyless. SunriseSunset.io (https://api.sunrisesunset.io/json?lat=LAT&lng=LNG&date=YYYY-MM-DD) adds golden_hour field directly.
- **Recommendation:** Build now as daily enrichment — one call per day per location, write sunrise/sunset/golden_hour to existing weather/ or a new astronomy/ sub-directory. USNO is more authoritative and covers moon data too; prefer USNO if building both.
- **Notes:** USNO rstt/oneday endpoint (already listed) covers the same data plus moon data; use USNO as the primary source and sunrise-sunset.org only as a fallback. Both are keyless. Golden hour is ~±1 hour around sunrise/sunset; SunriseSunset.io returns it explicitly.

#### USNO Moon Phases API — _Daylight & Astronomy_

🟢 **High — same USNO service as sunrise/sunset (already listed), trivial to add moon phase pull in the same daily astronomy collector.** · M5 · none · effort **S** · 🆕 new

- **Access:** https://aa.usno.navy.mil/api/moon/phases/year?year=YYYY — returns full year of primary lunar phases (new, first quarter, full, last quarter) with UTC times. https://aa.usno.navy.mil/api/moon/phases/date?date=YYYY-MM-DD&nump=N — N phases from a starting date. Keyless.
- **Recommendation:** Build now — pull once per day (or just the year's phases at year boundary), store in environment/astronomy/moon-phases.jsonl. Very low call volume.
- **Notes:** Combines naturally with the USNO rstt/oneday response which also returns moon illumination fraction and moonrise/moonset. A single daily USNO call covering rstt/oneday + moon/phases/date gives a complete daylight + moon picture at ~2 API calls/day.

#### NOAA National Data Buoy Center (NDBC) — ocean/marine observations — _Marine_

🟡 **Medium — keyless, but data is plain-text tabular files (not JSON); requires custom parser. US coastal and Great Lakes coverage; ~1,000 active stations.** · M5 · none · effort **M** · 🆕 new

- **Access:** https://www.ndbc.noaa.gov/data/realtime2/STATIONID.txt — standard meteorological file (updated hourly). Station list: https://www.ndbc.noaa.gov/activestations.xml. Data includes: wave height, wave period, swell, wind speed/direction, air/water temp, pressure. Keyless text files.
- **Recommendation:** Build later — useful for coastal users wanting actual buoy observations rather than model forecasts. Narrow audience. Parse the .txt fixed-width format into JSONL.
- **Notes:** Complementary to Open-Meteo Marine (model forecast) — NDBC gives real-time observed data from physical buoys. 45 days rolling for standard met files. Historical archives at ndbc.noaa.gov/historical_data.shtml going back decades.

#### Apple Health export.zip — Environmental Audio Exposure — _Ambient Sound_

🟢 **High — data is already in the existing Apple Health export.zip import path. Requires adding two new metric types to the health importer.** · M1 · none (user-initiated export from iPhone) · effort **S** · 🆕 new

- **Access:** Inside Apple Health export.zip (iPhone Health app → share → Export All Health Data): export.xml contains HKQuantityTypeIdentifierEnvironmentalAudioExposure (dB SPL, measured by Apple Watch microphone) and HKCategoryTypeIdentifierEnvironmentalAudioExposureEvent (loud environment event triggers). Imported alongside existing Apple Health importer (M1 path already built).
- **Recommendation:** Build now — zero infrastructure cost, just add parsing for the two audio exposure identifiers in the existing health.rs importer. Requires Apple Watch to generate data.
- **Notes:** Apple Watch only — users without Apple Watch will have no data for these fields. environmentalAudioExposure is sampled continuously when Watch is worn; event type fires only when dB sustained above threshold. Data lives in the same export.xml already being parsed. Store as health/environmental-audio/ in the vault alongside other health metrics.

#### macOS Microphone — Live Ambient dB Level — _Ambient Sound_

🟡 **Medium — API exists and works on macOS. Important caveat: consumer laptop microphones have inconsistent frequency response and pre-amp gain, so absolute dB SPL values are unreliable for comparison across devices. Values are useful for relative trend tracking (quiet vs. loud environment) but not calibrated.** · M4 · TCC Microphone (Privacy → Microphone in System Settings) · effort **M** · 🆕 new

- **Access:** AVAudioEngine / AVAudioInputNode on macOS with kAudioQueueProperty_CurrentLevelMeterDB for RMS power. TCC microphone permission required. Samples raw dB from built-in mic continuously.
- **Recommendation:** Spike first — consider privacy sensitivity of continuous microphone access carefully. If built, sample 5-second RMS windows every 5 minutes max, never record audio content, store only dB level in environment/ambient-sound/YYYY-MM.jsonl. Privacy disclosure in UI is essential.
- **Notes:** TCC microphone permission is a significant ask — users may be wary of apps with mic access. Unlike phone-based noise apps (Apple Watch does this well), a Mac mic is typically positioned differently and more affected by fan noise, keyboard typing, etc. Consider making this opt-in with explicit disclosure. Apple Watch health export (M1, above) is a better source of environmental noise data if the user has an Apple Watch.

#### NOAA Climate Data Online (CDO) — Historical Weather Observations — _Weather_

🟡 **Medium — useful for one-time historical backfill of observed (vs. reanalysis) weather data from actual nearby NOAA stations. Open-Meteo ERA5 archive (already planned) covers same use case with zero auth; CDO adds station-specific ground truth.** · M5 · API key (free, instant email registration) · effort **M** · 🆕 new

- **Access:** https://www.ncei.noaa.gov/cdo-web/api/v2/data?datasetid=GHCND&locationid=FIPS:STATE&startdate=YYYY-MM-DD&enddate=YYYY-MM-DD&limit=1000 — requires free token from ncdc.noaa.gov/cdo-web/token. 5 req/sec, 10k req/day. Returns daily observations from GHCND (Global Historical Climatology Network) — temp max/min, precip, snow, wind.
- **Recommendation:** Icebox — Open-Meteo's historical archive (ERA5, back to 1940, keyless) covers the historical backfill use case adequately. CDO is more authoritative for raw station observations but adds API key complexity and query complexity (station discovery required).
- **Notes:** Best use case is verifying historical records against a specific NOAA station. Open-Meteo ERA5 reanalysis already available without key. Key takes a few minutes to arrive by email. Rate limits are generous for one-time backfill.

#### PurpleAir API (Hyperlocal PM2.5 from community sensors) — _Air Quality_

🟡 **Medium — free key, excellent data density in metro areas, 2-minute update rate. Sensor density is uneven (dense in wealthy US/EU cities, sparse elsewhere).** · M5 · API key (free, Google SSO signup) · effort **M** · 🆕 new

- **Access:** https://api.purpleair.com/v1/sensors?fields=pm2.5,temperature,humidity&location_type=0&nwlng=LON1&nwlat=LAT1&selng=LON2&selat=LAT2 — returns nearby sensor readings. API Read Key required, obtained free from develop.purpleair.com (Google SSO).
- **Recommendation:** Build later — hyperlocal PM2.5 from nearby community sensors is genuinely more accurate than model data in urban areas. But coverage is US/EU-centric and requires an API key. Good follow-on after AirNow + Open-Meteo are in place.
- **Notes:** PurpleAir uses dual-channel laser particle counters; EPA correction factors applied in API output option ('correction=EPA'). Sensors date back to 2016. Historical queries available. The develop.purpleair.com portal generates keys via Google SSO — no credit card. Users near no PurpleAir sensors get empty results gracefully.

#### Google Maps Pollen API (US/global pollen forecast) — _Pollen / Allergy_

🟡 **Medium — best US pollen forecast coverage and global reach (65+ countries), but requires Google billing account setup. Free tier is 5,000 calls/month (sufficient at 1 call/day = ~365/year).** · M5 · API key (Google Cloud billing required, free tier: 5,000/month per SKU) · effort **M** · 🆕 new

- **Access:** https://pollen.googleapis.com/v1/forecast:lookup?location.longitude=LON&location.latitude=LAT&days=5&key=API_KEY — returns daily pollen forecast for tree, grass, weed with species breakdown and UPI index. Requires Google Cloud billing account + API key.
- **Recommendation:** Build later — best global pollen coverage. The billing account requirement adds friction but the free tier is adequate for personal use. For Europe, Open-Meteo pollen (keyless) is sufficient; Google Pollen adds US and global coverage.
- **Notes:** Introduced ~2023, covers 65+ countries. Returns pollen UPI (Universal Pollen Index) plus species-level breakdown (e.g., oak, birch, ragweed). Billing required even for free tier — user must connect a Google Cloud account. For users willing to set this up, superior to all alternatives for US pollen data. Alternative: Ambee pollen API (key required, paid above free tier).

#### Blitzortung / LightningMaps (lightning strike detection) — _Lightning_

🟠 **Low — usage policy requires third-party apps to serve data from their own servers rather than direct client-to-broker connections; not suitable for a local-first app where each instance hits the broker directly. Commercial alternatives (OpenWeather Lightning, Xweather, Vaisala) require paid keys.** · M5 · none (MQTT public broker; HTTP endpoint public) · effort **L** · 🆕 new

- **Access:** Blitzortung data served via MQTT at a public broker (data.blitzortung.org) with geohash-based topics. Third-party apps must proxy through own servers per usage policy. HTTP endpoint: data.blitzortung.org/Data/Protected/last_strikes.php (100k recent strikes with detector positions). Community-operated, open data.
- **Recommendation:** Skip — usage policy friction makes direct integration infeasible for a local-first app. NWS severe weather alerts (already recommended) cover thunderstorm warnings adequately. If lightning strike proximity becomes a user request, spike Blitzortung MQTT proxying or evaluate OpenWeather Lightning ($) at that point.
- **Notes:** Blitzortung is a community volunteer network of ~3,000 sensors globally. The data is excellent and open but their distribution policy is designed to prevent individual clients hammering the central broker. OpenWeather Lightning API requires a paid subscription. The NWS alerts API (keyless) covers severe thunderstorm warnings as a functional substitute.

#### macOS CoreMotion barometric pressure (CMAltimeter) — _Weather_

🔴 **Blocked — CMAltimeter is not available on macOS regardless of hardware. Macs with Apple Silicon do contain barometer hardware but Apple does not expose it via public APIs on macOS. Barometric pressure is available from Open-Meteo forecast API (pressure_msl field already in the weather struct).** · M4 · N/A · effort **XL** · 🆕 new

- **Access:** CoreMotion.CMAltimeter on iOS/watchOS. On macOS: NOT available — the framework is present in the SDK for Mac Catalyst but the altitude/pressure APIs are marked unavailable on macOS. Confirmed blocked per Apple Developer Forums.
- **Recommendation:** Skip — use Open-Meteo's pressure_msl (mean sea level pressure) which is already collected in the existing weather stream.
- **Notes:** Apple Watch CMAltimeter is accessible via HealthKit export (atmospheric_pressure samples in export.xml) if the user has an Apple Watch and exports Health data. This is a viable alternative path for Apple Watch owners but adds no new integration complexity beyond the existing Health import.

#### Open-Meteo Climate Change API (CMIP6 projections) — _Climate_

🟡 **Medium — keyless and technically feasible, but more useful as a one-time analytical query than a recurring collection target. Projections (future) and bias-corrected historical (past) rather than observed conditions.** · M5 · none · effort **M** · 🆕 new

- **Access:** https://climate-api.open-meteo.com/v1/climate?latitude=LAT&longitude=LON&start_date=YYYY-MM-DD&end_date=YYYY-MM-DD&models=CMCC_CM2_VHR4&daily=temperature_2m_max,precipitation_sum — daily climate projections 1950–2050 at 10 km resolution. No key.
- **Recommendation:** Icebox — interesting for 'how has my local climate changed' analysis but not a live ambient data stream. Better exposed as an on-demand query in the analysis UI than a scheduled collector.
- **Notes:** Covers 1950–2050 under CMIP6 scenarios. Seven models available. Statistically bias-corrected against ERA5. Not a substitute for actual historical weather observations but useful for climate trend analysis. Projections beyond 2050 not available in this API.

### Environment & Ambient Context — cross-cutting notes

1. KEYLESS-FIRST CLUSTER: Open-Meteo (weather, air quality, marine, flood), USNO (astronomy), USGS (earthquakes), NOAA CO-OPS (tides), NOAA SWPC (space weather), NWS api.weather.gov (alerts), USGS Water Services (stream gauges), and sunrise-sunset.org are ALL truly keyless public APIs. They share the same HTTP GET + JSON pattern and can all be handled by a single generic HTTP client in trove-core. A shared `environment::FetchClient` (or reuse of the existing Oura/TickTick OAuth client without auth) covers all of them. This is Trove's best domain for zero-friction, any-user integration.

2. HISTORICAL BACKFILL UNIVERSALLY AVAILABLE: All environment data is re-fetchable — no urgency class (unlike iMessage or play scrobbles that are lost if not captured live). Open-Meteo ERA5 back to 1940, USGS earthquakes open-ended, NOAA tides decades, NOAA SWPC Kp index decades. This means collectors can safely run lazy backfill: on first install, pull the last N months/years of data in a background pass.

3. SHARED LOCATION DEPENDENCY: All environment sources need a lat/lon. The existing CoreLocation bridge (corelocation.rs) + manual weather-location.json config already provides this. The pattern established for weather (CoreLocation → manual → cached) should be reused as-is across all environment collectors. Location is the one shared bottleneck — no location = no environment data.

4. OPEN-METEO AS ONE MULTI-ENDPOINT CALL: The existing weather collector uses one Open-Meteo /v1/forecast call. UV index, sunrise, sunset, pressure_msl, and several other missing ambient variables can be added to the existing hourly/daily parameter lists with zero additional HTTP calls. The air quality, marine, and flood endpoints are separate Open-Meteo sub-APIs (different hostnames) but the same JSON schema and zero-key pattern. Treat as one logical family with a shared base client.

5. US vs. GLOBAL SPLIT: NWS alerts, AirNow, NOAA CO-OPS tides, USGS earthquakes/water, and NDBC buoys are US-only or US-primary. Open-Meteo family, USNO, USGS earthquakes (global catalog), WAQI, and NOAA SWPC are global. Design collectors to detect location and select appropriate source; always include the Open-Meteo family as the global baseline fallback. Never hard-fail for non-US users — degrade gracefully to available global data.

6. AMBIENT SOUND IS THE ODD ONE OUT: Unlike all other environment sources (network APIs with no permission), ambient sound via the Mac microphone requires TCC Microphone permission and has genuine privacy sensitivity. It is also the least reliable (uncalibrated laptop mic). Apple Watch noise export (M1) is a superior and privacy-safer path for users who have an Apple Watch. Consider these two paths mutually exclusive with Watch export preferred.

7. VAULT LAYOUT SUGGESTION: Add an `environment/` top-level directory alongside `weather/`. Sub-directories: `air-quality/`, `earthquakes/`, `tides/`, `space-weather/`, `wildfire/`, `flood/`, `alerts/`, `astronomy/`. The existing `weather/` directory stays as-is (already built). This matches the pattern of `health/`, `activity/`, `browser/`, etc.

---

## Geolocation & Travel

This domain covers where the user goes — raw GPS trails, visited places, exercise routes, flights, transit, car trips, and derived travel records (hotel stays, lodging, loyalty programs). The domain is split between high-value, always-on passive location capture (Google Timeline, Apple Significant Locations, phone GPS loggers) and episodic structured records (flights, hotels, check-ins, transit). Most high-value sources are either on-device local databases or OAuth cloud APIs — both fit Trove's model well. The primary blocker class is Apple's encryption of its own on-device location stores (Significant Locations and Maps Visited Places are end-to-end encrypted and unreadable by any third party, including Trove). The secondary blocker class is services with no API and no export (transit card history, hotel booking history for guests). Net-new high-value sources versus the seed list: Garmin Connect (bulk export + unofficial API), Wahoo Fitness (official OAuth API), myFlightRadar24 (CSV export), Flighty macOS SQLite local DB, and flight confirmation email parsing as a derived source for flights.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Strava | Fitness GPS | M5; M1 fallback | OAuth (athlete:read_all scope); bring-your-own or compiled-in app credentials | M | 🟢 High — well-documented public OAuth API, 200 req/15 min, 2000/day. API Agreement updated June 2026; still live. GPS polylines + full streams accessible. Bulk export M1 path covers history without API quota concerns. | 📋 planned |
| Google Timeline / Location History | Continuous GPS Trail | M1 | none (user manually exports from phone) | M | 🟢 High — JSON export is user-accessible, format is documented by community. However, iOS users may have a different/reduced export format vs Android. Takeout no longer works for this data. | 📋 planned |
| Garmin Connect | Fitness GPS | M1; M5 blocked without partnership | none for M1 export; OAuth + partnership application for official API | S | 🟢 High for M1 bulk export — Garmin's official export feature provides all FIT/GPX files. Official API requires partnership (push-based, not self-serve), making ongoing sync hard. Unofficial scrapers are fragile. | 🆕 new |
| Flighty (macOS) | Flight Tracker | M3 | none (app container path is user-accessible without FDA) | S | 🟢 High — local SQLite path is confirmed, schema discoverable, no encryption. Only works if the user has the Flighty app installed; gracefully skip if DB absent. | 🆕 new |
| Overland (iOS GPS Logger) | Continuous GPS Trail | M2 | none (Trove receives; user configures Overland to point at local endpoint) | M | 🟡 Medium — requires user to install Overland on iPhone AND configure the endpoint. But for power users who want always-on GPS tracking this is the best open-source path. The receiver endpoint must be reachable from the iPhone (same WiFi, or tunneled). | 📋 planned |
| OwnTracks | Continuous GPS Trail | M2 | none (Trove receives) | M | 🟡 Medium — same class as Overland, requires user setup. HTTP mode is simplest. iOS 16+ minimum. | 📋 planned |
| Arc Timeline (iOS) | Continuous GPS Trail + Place Classification | M1 | none (user exports from app) | L | 🟡 Medium — Arc has rich classified data (visits with place names, trips with transport mode, GPS tracks) but no documented bulk export or API as of 2026. A spike is needed to determine what export formats exist in Arc 4. | 📋 planned |
| myFlightRadar24 | Flight Log | M1 | none (web export, requires Flightradar24 account) | S | 🟢 High — official CSV export is documented and working. No API required. Many flight-tracking users maintain their logbook on myFlightradar24. | 🆕 new |
| Wahoo Fitness | Fitness GPS | M5 | OAuth (self-serve app registration at cloud-api.wahooligan.com) | M | 🟢 High — official self-serve OAuth API, well documented, FIT file downloads available. Covers ELEMNT GPS computers and KICKR trainers. | 🆕 new |
| Tesla Fleet API | Car Telemetry | M5 | OAuth (Tesla developer app registration required; vehicle_location scope mandatory since Jan 2025) | L | 🟡 Medium — official API exists and is well documented, but Fleet Telemetry (for live streaming) requires a persistent server endpoint that vehicles stream to, which conflicts with Trove's local-first/standalone model. Polling vehicle_data endpoint works but gives point-in-time location, not trip history. | 🆕 new |
| Smartcar | Car Telemetry (Multi-Brand) | M5 | OAuth (self-serve registration; user connects vehicle during onboarding) | M | 🟡 Medium — API is live and well documented for current location + odometer. Does NOT provide trip history — only current/latest known values. No historical location log. | 🆕 new |
| Swarm (Foursquare) | Check-ins | M5 | OAuth (user grants access to their own check-in history) | M | 🟡 Medium — API endpoint appears live but some users report 402 errors; Foursquare support confirmed users should not be charged for their own data. The API is undocumented-ish and Foursquare's developer focus has shifted. Some users export via OwnYourSwarm or third-party tools. | 🆕 new |
| Apple Health Export (Workout Routes / GPX) | Fitness GPS | M1 | none (already in the export.zip the user imports) | S | 🟢 High — data is already present in the export ZIP Trove already processes. Just needs the GPX files parsed and stored as vault GPS tracks. | 📋 planned |
| Apple Maps Visited Places (iOS 26) | Place Visits | M3 | Blocked — data is end-to-end encrypted on-device on iPhone (not Mac); not accessible to third-party apps or Trove on macOS | XL | 🔴 Blocked — end-to-end encryption means the on-device DB is unreadable by Trove even with Full Disk Access. No export feature exists as of 2026. macOS does not have the Maps Visited Places feature. The only possible path would be a companion iOS app within Apple's HealthKit/CoreLocation entitlement framework, which is out of scope. | 🆕 new |
| Apple Significant Locations (macOS) | Frequent Places | M3 | Blocked — databases are encrypted with device keys; FDA is insufficient | XL | 🔴 Blocked — Apple explicitly states Significant Locations are end-to-end encrypted and unreadable by Apple or any third party. The 'cache_encrypted' filenames reflect reality. Full Disk Access would let you open the file but the content is AES-encrypted with a key stored in the device's Secure Enclave. No third-party path exists on macOS. | 🆕 new |
| TripIt | Trip Itinerary | M5 | OAuth (no new app registrations accepted as of 2025; existing keys only) | L | 🟠 Low — no new API access available. Users can export a TripIt account via Settings → Download Your Data (produces ICS/PDF) but no machine-readable JSON export. Parent company SAP Concur has a separate API but is enterprise-focused. | 📋 planned |
| Flight Confirmation Email Parsing | Flight Records (Derived) | M1 | none (processes already-imported email .mbox files) | M | 🟢 High — confirmation emails from major US and international airlines follow predictable patterns. Many attach ICS files with structured FlightReservation data. The AwardWallet Email Parsing API (awardwallet.com/api/main) is a commercial API that does exactly this parsing if a managed solution is preferred. | 🆕 new |
| Garmin Connect Bulk Export | Fitness GPS (Export) | M1 | none (requires Garmin Connect account) | S | 🟢 High — official Garmin export feature, well documented, free. FIT files are the gold standard for GPS + biometric activity data. | 🆕 new |
| Life360 | Family Location Sharing | M5 | OAuth (unofficial; no export option; credentials required) | L | 🟠 Low — unofficial API only, actively blocked by Life360/Cloudflare at times. No export feature. Life360 has also acquired Tile (device tracker) and has had data-selling controversies. Using an unofficial API is fragile and terms-of-service grey area. | 🆕 new |
| AwardWallet (Loyalty Programs) | Frequent Flyer / Loyalty | M5; M1 fallback | OAuth + paid AwardWallet Business subscription for full travel timeline | M | 🟡 Medium — API is live and commercially available but requires a paid AwardWallet Business account for the travel timeline export. The Email Parsing API may have a free tier for personal use. Good for mileage/points tracking. | 🆕 new |
| Airbnb (Guest Booking History) | Lodging Records | M1 | none (requires Airbnb account login, manual web export) | S | 🟡 Medium — a guest booking CSV export exists but requires manual web download. No API for guest data. The May 2025 calendar export removal was for hosts; guest booking CSV may still work but is not well documented. | 🆕 new |
| Transit Card History (Clipper, Oyster, ORCA, etc.) | Transit Commute | M1 | none (requires transit card account login, manual web download) | L | 🟠 Low — most transit cards offer PDF-only exports or no export at all. No public APIs. Community advocacy (the 2019 Clipper API proposal) has not resulted in an API. Each card operator would need a separate PDF parser. | 🆕 new |
| Google Maps Visited Places (Apple Maps analog) | Place Visits | M1 | none (Google Takeout export, requires Google account) | S | 🟢 High — Takeout still exports Saved Places (distinct from Timeline which moved on-device). GeoJSON format is easy to parse. | 🆕 new |

### Detail

#### Strava — _Fitness GPS_

🟢 **High — well-documented public OAuth API, 200 req/15 min, 2000/day. API Agreement updated June 2026; still live. GPS polylines + full streams accessible. Bulk export M1 path covers history without API quota concerns.** · M5; M1 fallback · OAuth (athlete:read_all scope); bring-your-own or compiled-in app credentials · effort **M** · 📋 planned

- **Access:** OAuth 2.0 — POST https://www.strava.com/oauth/token; activities list GET /api/v3/athlete/activities; per-activity GPS stream GET /api/v3/activities/{id}/streams?keys=latlng,time,altitude; bulk export also available via Settings > My Account > Download or Delete Your Data (ZIP with FIT/GPX files, emailed within hours)
- **Recommendation:** Build now — very high value, clear GPS + activity overlap with existing Health tab workout data. Webhook support means new activities can arrive in near real-time.
- **Notes:** Rate limits are per-athlete (200/15 min, 2000/day) — comfortable for personal use. Webhook requires a publicly reachable callback URL for live push; for a local app, polling on a cron is fine for personal use. Club endpoints being removed Sep 2026 — irrelevant for personal data. Bulk export ZIP contains original FIT/GPX files; prefer M1 for initial backfill, M5 for ongoing sync. As of June 2026, segment exploration endpoint being restricted to Extended Access Tier — irrelevant for personal activity data.

#### Google Timeline / Location History — _Continuous GPS Trail_

🟢 **High — JSON export is user-accessible, format is documented by community. However, iOS users may have a different/reduced export format vs Android. Takeout no longer works for this data.** · M1 · none (user manually exports from phone) · effort **M** · 📋 planned

- **Access:** On-device export: Google Maps app (iOS or Android) → Profile icon → Your Timeline → Settings → Export Timeline data → Save. Produces Timeline.json on device. Format: semanticSegments (place visits + activity segments with timelinePath points), rawSignals. Community-maintained schema at locationhistoryformat.com. Legacy Takeout no longer provides location history (Google moved to on-device storage in 2024-2025).
- **Recommendation:** Build now — rich semantic place-visit data (place name, coordinates, duration, activity type) combined with raw GPS path is uniquely valuable for a life-logging vault. Parser is M effort given community schema docs.
- **Notes:** Data is now local-only on mobile device (post-2024 Google privacy change). iOS export reportedly less complete than Android (semanticSegments may be missing on iOS). Community schema at locationhistoryformat.com documents both old Takeout format and new on-device format — Trove should accept both for users migrating from old exports. Point format is strings like '50.0506312°, 14.3439906°' not numbers — parser must handle this. The Timelinize open-source project (Go) supports both formats and is a useful reference for the parser.

#### Garmin Connect — _Fitness GPS_

🟢 **High for M1 bulk export — Garmin's official export feature provides all FIT/GPX files. Official API requires partnership (push-based, not self-serve), making ongoing sync hard. Unofficial scrapers are fragile.** · M1; M5 blocked without partnership · none for M1 export; OAuth + partnership application for official API · effort **S** · 🆕 new

- **Access:** Bulk export: Garmin Connect web → Account Settings → Export Your Data → button triggers full archive ZIP (all FIT + GPX + TCX files, emailed within 24-48 hours). Official Activity API: developer.garmin.com/gc-developer-program/activity-api/ — push-based OAuth 1.0a, requires partnership application. Third-party unofficial scraper (garmin-connect-export GitHub) uses web session auth but may be blocked by TLS fingerprinting.
- **Recommendation:** Build now (M1 only) — many users have years of Garmin GPS data. FIT file parser (fitparser crate in Rust) gives full GPS + heart rate + power data. Skip the official API partnership path; accept the manual re-export workflow for updates, or let the user set up a periodic export.
- **Notes:** FIT format is Garmin's binary format — use the fitparser crate. GPX/TCX are also in the export. Activities cover cycling, running, swimming, hiking with full GPS traces. Official API is OAuth 1.0a (unusual), push-based (Garmin sends to your server), and requires a formal partnership application — not feasible for a standalone personal app. Unofficial scrapers (garmin-connect-export) authenticate via web session but have been intermittently blocked by Garmin's TLS fingerprinting; treat as fragile M6. Recommend: M1 bulk export as the canonical path.

#### Flighty (macOS) — _Flight Tracker_

🟢 **High — local SQLite path is confirmed, schema discoverable, no encryption. Only works if the user has the Flighty app installed; gracefully skip if DB absent.** · M3 · none (app container path is user-accessible without FDA) · effort **S** · 🆕 new

- **Access:** Local SQLite DB at ~/Library/Containers/com.flightyapp.flighty/Data/Documents/MainFlightyDatabase.db — confirmed by the flighty-mcp open-source project. Contains upcoming and past flights, flight status, delays, gates, weather, airline/airport info. Flighty also has a CSV export feature (Settings > Export). App also supports TripIt sync and calendar sync for import.
- **Recommendation:** Build now — flight history is high-value travel data, the local SQLite path is confirmed and readable without special permissions. Many Mac users who track flights use Flighty. The M3 approach is clean and zero-auth.
- **Notes:** Flighty is iOS-first but has a macOS app (available on Mac App Store). DB path ~/Library/Containers/com.flightyapp.flighty/Data/Documents/MainFlightyDatabase.db confirmed from the flighty-mcp GitHub project. Flighty also exports CSV via the app UI — M1 fallback for users on iOS-only. Schema must be reverse-engineered (no public docs). Flighty supports import from myFlightRadar24, App in the Air, OpenFlights, FlightMemory — so it can serve as an aggregator for flight history from multiple sources.

#### Overland (iOS GPS Logger) — _Continuous GPS Trail_

🟡 **Medium — requires user to install Overland on iPhone AND configure the endpoint. But for power users who want always-on GPS tracking this is the best open-source path. The receiver endpoint must be reachable from the iPhone (same WiFi, or tunneled).** · M2 · none (Trove receives; user configures Overland to point at local endpoint) · effort **M** · 📋 planned

- **Access:** Overland iOS app (open source, Apache 2.0, github.com/aaronpk/Overland-iOS) sends location batches via HTTP POST to a user-configurable endpoint. Trove could expose a local receiver endpoint (e.g. via the troved HTTP server) OR the user can configure Overland to post to a local server on their network. Payload is GeoJSON FeatureCollection. The app saves data offline and batches sends.
- **Recommendation:** Build later — very useful for power users but requires iPhone app setup + network configuration. Lower priority than importing existing location data. When built, troved should expose an /overland endpoint and parse incoming GeoJSON into vault JSONL.
- **Notes:** Overland supports both native Overland format and OwnTracks format. The app batches location data while offline. GeoJSON payload includes coordinates, timestamp, speed, altitude, battery level, wifi SSID, motion type. This is the canonical self-hosted GPS logger for IndieWeb users. Similar option: OwnTracks (also open source, supports MQTT or HTTP, supports iOS and Android) — same M2 mechanism, slightly different payload format. Both are worth supporting with the same receiver.

#### OwnTracks — _Continuous GPS Trail_

🟡 **Medium — same class as Overland, requires user setup. HTTP mode is simplest. iOS 16+ minimum.** · M2 · none (Trove receives) · effort **M** · 📋 planned

- **Access:** OwnTracks iOS/Android app (open source) in HTTP mode POSTs JSON location payloads to user-configured endpoint. Payload includes lat/lon/timestamp/accuracy/battery/velocity/altitude plus optional region enter/leave events. Significant Location Change mode reduces battery drain. Can run in MQTT mode (requires broker) or HTTP mode (simpler).
- **Recommendation:** Build later — implement alongside Overland receiver since both are HTTP-based GPS logger apps with similar payloads. Share the same troved endpoint with format detection.
- **Notes:** OwnTracks Recorder (github.com/owntracks/recorder) is a reference server implementation in C. For Trove, implement a minimal HTTP receiver in troved. OwnTracks payload is JSON with _type:location, lat, lon, tst (Unix timestamp), acc, batt, vel, alt, topic. The iOS app was updated for iOS 18 (Xcode 16) in 2025 and is actively maintained.

#### Arc Timeline (iOS) — _Continuous GPS Trail + Place Classification_

🟡 **Medium — Arc has rich classified data (visits with place names, trips with transport mode, GPS tracks) but no documented bulk export or API as of 2026. A spike is needed to determine what export formats exist in Arc 4.** · M1 · none (user exports from app) · effort **L** · 📋 planned

- **Access:** Arc Timeline app (bigpaua.com) — iOS only — automatically classifies movements into visits and trips with activity type detection. Arc Timeline 4 is current (2025 rebuild). No public API documented. The app has local SQLite storage on-device. Export: Arc has historically supported GPX export per-item but no bulk export API. The developer has indicated an export feature is on the roadmap.
- **Recommendation:** Spike first — Arc's classified place visit + trip data is higher quality than raw GPS logs (it identifies 'home', 'office', transport modes) but the export story is unclear in 2026. Check if Arc 4 added export. If only per-item GPX, the ROI is low for bulk import.
- **Notes:** Arc Timeline 4 was released in 2025 as a ground-up rebuild. The older Arc iOS app (1063151918 on App Store) had a SQLite database at ~/Documents/Arc Timeline.sqlite accessible via iTunes file sharing, but Arc 4 may have changed this. The developer (Matt Greenfield / Big Paua) is responsive to feature requests. Arc's ML-classified visit/trip data is uniquely valuable — worth a spike to find the export path.

#### myFlightRadar24 — _Flight Log_

🟢 **High — official CSV export is documented and working. No API required. Many flight-tracking users maintain their logbook on myFlightradar24.** · M1 · none (web export, requires Flightradar24 account) · effort **S** · 🆕 new

- **Access:** Web export: login to my.flightradar24.com → Settings → Export → Download CSV. URL: https://my.flightradar24.com/settings/export. CSV contains date, origin, destination, airline, flight number, aircraft type. Free account supports this export. Also supports CSV import for bulk upload of past flights.
- **Recommendation:** Build now — trivial CSV import, widely used by flight enthusiasts. Complements Flighty M3 path. Date/origin/destination CSV is simple to parse into vault flight records.
- **Notes:** CSV format has Date, Origin, Destination as mandatory fields plus optional airline, flight number, aircraft. No official REST API for personal flight history — export only. The pyfr24 Python library can fetch live flight track data from the public Flightradar24 API but is about live aircraft, not personal history. Combine with Flighty DB read to give most users full flight coverage.

#### Wahoo Fitness — _Fitness GPS_

🟢 **High — official self-serve OAuth API, well documented, FIT file downloads available. Covers ELEMNT GPS computers and KICKR trainers.** · M5 · OAuth (self-serve app registration at cloud-api.wahooligan.com) · effort **M** · 🆕 new

- **Access:** OAuth 2.0 at api.wahooligan.com — scopes: workouts_read, offline_data. GET /v1/workouts returns workout list; GET /v1/workouts/{id} includes FIT file URL under 'file' key for download. Webhook: workout_summary events pushed on activity completion. Rate limit: 10 unrevoked tokens per user cap from 2026-01-01.
- **Recommendation:** Build later — valuable for users with Wahoo ELEMNT bike computers. FIT files give full GPS + power + heart rate. Lower priority than Strava (which many Wahoo users also sync to). If Strava is built first, Wahoo data often appears there via auto-sync.
- **Notes:** Wahoo auto-syncs to Strava, so building Strava first captures most Wahoo users. Build Wahoo direct for users who don't use Strava or want the raw FIT files. OAuth 1.0a is NOT used here (unlike Garmin) — Wahoo uses standard OAuth 2.0. Token cap of 10 per user is generous for personal use.

#### Tesla Fleet API — _Car Telemetry_

🟡 **Medium — official API exists and is well documented, but Fleet Telemetry (for live streaming) requires a persistent server endpoint that vehicles stream to, which conflicts with Trove's local-first/standalone model. Polling vehicle_data endpoint works but gives point-in-time location, not trip history.** · M5 · OAuth (Tesla developer app registration required; vehicle_location scope mandatory since Jan 2025) · effort **L** · 🆕 new

- **Access:** Tesla Fleet API at developer.tesla.com — OAuth 2.0 with vehicle_location scope (required since Jan 2025). Vehicle state: GET /api/1/vehicles/{id}/vehicle_data. Fleet Telemetry streaming: github.com/teslamotors/fleet-telemetry — requires a server endpoint that vehicles stream to directly (configurable fields including Location at 10-second intervals). App registration required at developer.tesla.com.
- **Recommendation:** Spike first — the streaming telemetry requires an always-on server with a public URL (not local-first compatible). The polling endpoint gives current location only, not historical trips. TeslaMate (open source, self-hosted) is a better reference: it absorbs telemetry into a local PostgreSQL DB. Consider reading TeslaMate's DB as M3 instead if user has TeslaMate running.
- **Notes:** Automatic (OBD dongle) shut down May 2020 — confirmed dead. Tesla does NOT store trip history in a user-accessible cloud endpoint; the vehicle_data endpoint is current state only. Fleet Telemetry (the streaming path) requires a TLS server endpoint that Tesla vehicles connect to — incompatible with purely local operation. TeslaMate (self-hosted Docker) captures all trip data to a local PostgreSQL DB; reading TeslaMate's DB as M3 is a pragmatic path for TeslaMate users. Smartcar (smartcar.com) provides GET /location and GET /odometer for many brands (Tesla, Ford, BMW, Hyundai, Toyota, GM, VW) via OAuth 2.0 — more feasible than Tesla direct for multi-brand coverage.

#### Smartcar — _Car Telemetry (Multi-Brand)_

🟡 **Medium — API is live and well documented for current location + odometer. Does NOT provide trip history — only current/latest known values. No historical location log.** · M5 · OAuth (self-serve registration; user connects vehicle during onboarding) · effort **M** · 🆕 new

- **Access:** OAuth 2.0 at smartcar.com — GET /v2.0/vehicles/{id}/location (lat/lon), GET /v2.0/vehicles/{id}/odometer (mileage). Supports 40+ OEMs. Webhook streaming also available. Self-serve developer registration.
- **Recommendation:** Build later — useful for odometer tracking (mileage log) and current location snapshot, but not a GPS trail source. Better for logging daily mileage than for trip reconstruction. Low priority vs. GPS-trail sources.
- **Notes:** Smartcar is the successor to Automatic in the connected-car API space. Covers Tesla, GM/Chevy/GMC, Ford, BMW, Hyundai/Kia, Toyota, VW, Mercedes-Benz, Stellantis (Jeep/Ram/Dodge), DS. Only provides latest location snapshot and odometer — no trip history or GPS trace replay. For trip reconstruction, the user would need periodic polling stored as a time series in Trove.

#### Swarm (Foursquare) — _Check-ins_

🟡 **Medium — API endpoint appears live but some users report 402 errors; Foursquare support confirmed users should not be charged for their own data. The API is undocumented-ish and Foursquare's developer focus has shifted. Some users export via OwnYourSwarm or third-party tools.** · M5 · OAuth (user grants access to their own check-in history) · effort **M** · 🆕 new

- **Access:** Foursquare/Swarm API: GET https://api.foursquare.com/v2/users/self/checkins with OAuth token. Returns check-in records with venue name, coordinates, category, timestamp. Foursquare City Guide app shut down Dec 15, 2024 and web May 2025; Swarm app is the surviving check-in product and its API endpoint remains live as of late 2025.
- **Recommendation:** Build later — check-in data is a nice 'places I've been' record but Swarm's user base has shrunk and the API reliability is uncertain. If built, add M1 export fallback (users can request data export from Foursquare settings). Worth doing after higher-value GPS sources.
- **Notes:** Foursquare City Guide is fully dead (Dec 2024 app, April 2025 web). Swarm is alive and focusing on check-ins. The v2 API endpoint /users/self/checkins still works with a user OAuth token as of December 2025 (confirmed by a blog post about exporting to Day One). Token acquisition requires OAuth dance. 402 errors have been reported but Foursquare acknowledged this as a bug. Alternative: request a GDPR data export from foursquare.com/download-data which returns JSON.

#### Apple Health Export (Workout Routes / GPX) — _Fitness GPS_

🟢 **High — data is already present in the export ZIP Trove already processes. Just needs the GPX files parsed and stored as vault GPS tracks.** · M1 · none (already in the export.zip the user imports) · effort **S** · 📋 planned

- **Access:** Apple Health export.zip already imported by Trove (built). The export ZIP contains workout route GPX files in apple_health_export/workout-routes/ directory, each a standard GPX file with timestamped trackpoints (lat/lon/ele). These are not currently parsed by Trove's health.rs importer.
- **Recommendation:** Build now — the data is already in the import flow, just not extracted yet. Parsing GPX (quick-xml in Rust) is straightforward. These tracks cover Apple Watch workouts with GPS (outdoor runs, walks, hikes, cycles). Store under `health/` with the workout record they belong to (records route whole — see the taxonomy table; the location view joins routes at read time).
- **Notes:** GPX files are in apple_health_export/workout-routes/ inside export.zip. Each file is a standard GPX 1.1 trackpoint sequence. Files are named like 'Route YYYY-MM-DD HH.MM.SS.gpx'. For workouts without GPS (treadmill, indoor cycle) there is no route file. This path is the primary source of GPS tracks for Apple Watch users who do not use Garmin or Strava. Extend the existing import_health_export function in health.rs to optionally extract and store route GPX files.

#### Apple Maps Visited Places (iOS 26) — _Place Visits_

🔴 **Blocked — end-to-end encryption means the on-device DB is unreadable by Trove even with Full Disk Access. No export feature exists as of 2026. macOS does not have the Maps Visited Places feature. The only possible path would be a companion iOS app within Apple's HealthKit/CoreLocation entitlement framework, which is out of scope.** · M3 · Blocked — data is end-to-end encrypted on-device on iPhone (not Mac); not accessible to third-party apps or Trove on macOS · effort **XL** · 🆕 new

- **Access:** iOS 26 feature (released 2025): opt-in place visit log stored on-device, end-to-end encrypted, not uploaded to Apple servers. UI: Maps app → Profile → Places → Visited Places. No documented export. Local DB: likely under /private/var/mobile/Library/Caches/com.apple.Maps/ on iPhone — unknown schema, possibly part of the routined ecosystem.
- **Recommendation:** Icebox — encryption is a hard wall. Monitor for an official export feature in future iOS versions (Apple may add GDPR-motivated export). Not actionable in 2026.
- **Notes:** iOS 26 introduced Visited Places as an opt-in feature in Maps (confirmed by MacRumors and 9to5Mac, March 2026). Data is on-device only, E2E encrypted. Not synced to iCloud in a readable form. The routined database (cache_encryptedA.db / cache_encryptedB.db) that underlies Significant Locations uses device-specific encryption keys — the same applies here. No known forensic tool can read this without the device unlocked and paired.

#### Apple Significant Locations (macOS) — _Frequent Places_

🔴 **Blocked — Apple explicitly states Significant Locations are end-to-end encrypted and unreadable by Apple or any third party. The 'cache_encrypted' filenames reflect reality. Full Disk Access would let you open the file but the content is AES-encrypted with a key stored in the device's Secure Enclave. No third-party path exists on macOS.** · M3 · Blocked — databases are encrypted with device keys; FDA is insufficient · effort **XL** · 🆕 new

- **Access:** macOS routined daemon stores significant location data in /private/var/folders/.../com.apple.routined/ — files include cache_encryptedA.db and cache_encryptedB.db. On iOS the equivalent is /private/var/mobile/Library/Caches/com.apple.routined/. Both are end-to-end encrypted with device-specific keys; forensic tools (ElcomSoft) cannot read them without device pairing. Apple's UI shows them at System Settings → Privacy & Security → Location Services → System Services → Significant Locations.
- **Recommendation:** Icebox — hard encryption wall, no workaround without a companion iOS app using private APIs. The Google Timeline import covers the same semantic (frequent places) for Android users; for iPhone users, CoreLocation + watch-folder from a GPS logger app is the path.
- **Notes:** The Mac Locations Scraper GitHub project (mac4n6/Mac-Locations-Scraper) targets iOS device backups via forensic extraction, not live macOS access. On macOS, locationd's database at /var/db/locationd/ contains clients.plist (readable) and opaque encrypted files. Apple's privacy white paper confirms device-key encryption. This is a genuine dead end, not just a permission issue.

#### TripIt — _Trip Itinerary_

🟠 **Low — no new API access available. Users can export a TripIt account via Settings → Download Your Data (produces ICS/PDF) but no machine-readable JSON export. Parent company SAP Concur has a separate API but is enterprise-focused.** · M5 · OAuth (no new app registrations accepted as of 2025; existing keys only) · effort **L** · 📋 planned

- **Access:** TripIt public API (tripit.github.io/api/doc/v1/) — OAuth 1.0a or web auth. Returns trips, segments (flights, hotels, car rentals, trains), with dates/times/confirmation numbers. HOWEVER: as of 2024-2025, TripIt closed the API to new integrations. Existing API connections continue to work but no new app registrations are accepted.
- **Recommendation:** Icebox — API is closed to new integrations. Accept the ICS calendar export as M1 fallback (Trove already planned calendar/ICS import). The flight confirmation email parser (see below) covers the same travel-record use case for users who forward confirmation emails to TripIt.
- **Notes:** TripIt's GitHub issue tracker (issue #288, May 2024) confirms the API is dead for new developers. The ICS export from TripIt settings produces calendar events for all trips — these can be ingested via the planned generic IMAP/ICS path. TripIt Pro features (real-time flight alerts) are not relevant for data capture. For new users, App in the Air and Flighty are more viable flight-tracking sources.

#### Flight Confirmation Email Parsing — _Flight Records (Derived)_

🟢 **High — confirmation emails from major US and international airlines follow predictable patterns. Many attach ICS files with structured FlightReservation data. The AwardWallet Email Parsing API (awardwallet.com/api/main) is a commercial API that does exactly this parsing if a managed solution is preferred.** · M1 · none (processes already-imported email .mbox files) · effort **M** · 🆕 new

- **Access:** Airlines send booking confirmation emails containing structured flight data (departure/arrival times, flight number, airport codes, booking reference). Many emails contain ICS attachments or schema.org FlightReservation markup. Trove's existing email .mbox import can surface these; a flight-record extractor can pattern-match on sender domains (aa.com, united.com, delta.com, etc.) and extract structured records.
- **Recommendation:** Build later — once the email .mbox import is stable, add a flight-extractor pass that identifies confirmation emails and writes structured flight records to a flights/ vault folder. Cover the major airlines (AA, Delta, United, Southwest, Alaska, British Airways, Lufthansa, Emirates, Air Canada) plus generic ICS attachment parsing. AwardWallet's Email Parsing API is a commercial managed option if the extraction logic becomes too complex.
- **Notes:** Airlines increasingly include schema.org/FlightReservation JSON-LD in email HTML body, which is machine-readable with no regex. ICS attachments in airline emails often contain VEVENT entries with structured departure/arrival. AwardWallet's free Email Parsing API tier may be sufficient for personal use. Google and Apple Mail clients also auto-extract flights from confirmation emails into Trips / Calendar — the same structured data is available. This source covers users who use Flighty/TripIt as the downstream aggregator AND users who don't use any flight app.

#### Garmin Connect Bulk Export — _Fitness GPS (Export)_

🟢 **High — official Garmin export feature, well documented, free. FIT files are the gold standard for GPS + biometric activity data.** · M1 · none (requires Garmin Connect account) · effort **S** · 🆕 new

- **Access:** Garmin Connect web (connect.garmin.com) → Account Settings → scroll to Export Your Data → Export Data button. Triggers a full archive ZIP delivered by email within 24-48 hours. Contains all original FIT files, activity metadata CSV, health data, routes, device settings. Individual activity export: activity page → gear icon → Export to GPX/TCX/FIT.
- **Recommendation:** Build now — accept the Garmin export ZIP as an import source. Many fitness users have years of GPS activities on Garmin. The fitparser Rust crate can decode FIT files. High overlap with Apple Health workout route import but covers users without Apple Watch.
- **Notes:** Export ZIP includes: Activities/ (FIT files), DI_CONNECT/ (summary CSVs), WorkoutFiles/ (custom workouts), Courses/ (planned routes). The DI_CONNECT/ folder has structured CSVs with activity summaries (start time, type, distance, duration, calories, avg heart rate). FIT files carry full GPS trace + all sensor data. 24-48 hour turnaround means this is not real-time but is a reliable one-time or annual import. For ongoing sync, Strava is the better path (Garmin auto-syncs to Strava, which has an API).

#### Life360 — _Family Location Sharing_

🟠 **Low — unofficial API only, actively blocked by Life360/Cloudflare at times. No export feature. Life360 has also acquired Tile (device tracker) and has had data-selling controversies. Using an unofficial API is fragile and terms-of-service grey area.** · M5 · OAuth (unofficial; no export option; credentials required) · effort **L** · 🆕 new

- **Access:** Life360 has an unofficial REST API (base URL: https://www.life360.com/v3) documented by community reverse-engineering. OAuth2 password grant flow. Endpoints: GET /circles (list family circles), GET /circles/{id}/members (members + current locations). No official public API. No export feature in the app.
- **Recommendation:** Skip — unofficial API is fragile, Life360 blocks third-party access, no export path, data-selling controversy makes it an unlikely user trust target. Users who care about location privacy are unlikely to use Life360.
- **Notes:** Life360 explicitly stated (2014 tweet) there is no way to export data from the app — still true in 2025. The unofficial API documented at krconv.github.io/life360-api-docs gives circle/member/location data but Life360 has intermittently blocked third-party access via Cloudflare. Life360 has been criticized for selling precise location data to data brokers — users privacy-conscious enough to use Trove are an unlikely overlap audience.

#### AwardWallet (Loyalty Programs) — _Frequent Flyer / Loyalty_

🟡 **Medium — API is live and commercially available but requires a paid AwardWallet Business account for the travel timeline export. The Email Parsing API may have a free tier for personal use. Good for mileage/points tracking.** · M5; M1 fallback · OAuth + paid AwardWallet Business subscription for full travel timeline · effort **M** · 🆕 new

- **Access:** AwardWallet has three APIs: (1) Account Access API — OAuth to read loyalty account balances + travel reservations for AwardWallet users (awardwallet.com/api/account); (2) Web Parsing API — provide loyalty program credentials, returns balance + history; (3) Email Parsing API — extract travel reservations from confirmation email bodies (awardwallet.com/api/main). Paid Business subscription required for travel timeline export. Tracks 700+ loyalty programs.
- **Recommendation:** Build later — loyalty program balance tracking (miles, hotel points) is useful for users who are frequent travelers. The Email Parsing API is the most accessible entry point. Build after flight record import is stable, as flights provide the structural data that loyalty points relate to.
- **Notes:** AwardWallet covers Marriott Bonvoy, Hilton Honors, IHG, Hyatt, American AAdvantage, United MileagePlus, Delta SkyMiles, Southwest Rapid Rewards, Alaska Mileage Plan, and 690+ others. The Web Parsing API uses stored loyalty credentials to scrape balances — privacy-sensitive but opt-in. For Trove, the Email Parsing API (extracts reservation data from forwarded confirmation emails) is the most privacy-compatible path. Monthly account balance snapshots could track earning/burning of miles over time.

#### Airbnb (Guest Booking History) — _Lodging Records_

🟡 **Medium — a guest booking CSV export exists but requires manual web download. No API for guest data. The May 2025 calendar export removal was for hosts; guest booking CSV may still work but is not well documented.** · M1 · none (requires Airbnb account login, manual web export) · effort **S** · 🆕 new

- **Access:** Airbnb web: Login → Trips → Past → scroll to bottom → 'See all reservations' → CSV export available. Contains check-in/check-out dates, property name, city, confirmation code, price. No public guest API. Calendar export (ICS) was removed in a May 2025 update. Host CSV export exists separately under Today → See all reservations.
- **Recommendation:** Build later — useful for travel history (where you stayed, when, price) but requires manual export and Airbnb's web UI is not stable for scraping. Accept the CSV export if a user provides it. Low priority vs. flight and GPS sources.
- **Notes:** Airbnb does not provide a public API for guest booking history. The CSV export (if available) contains: confirmation code, check-in, check-out, listing name, city, country, amount paid. Airbnb's official data download under Privacy Settings → Request Your Personal Data produces a ZIP with reservations.json — this is the more reliable M1 path and includes all booking history. The guest booking CSV export availability may vary by account region.

#### Transit Card History (Clipper, Oyster, ORCA, etc.) — _Transit Commute_

🟠 **Low — most transit cards offer PDF-only exports or no export at all. No public APIs. Community advocacy (the 2019 Clipper API proposal) has not resulted in an API. Each card operator would need a separate PDF parser.** · M1 · none (requires transit card account login, manual web download) · effort **L** · 🆕 new

- **Access:** Varies by card. Clipper (Bay Area): web account at clippercard.com shows transaction history by date range; exports PDF only — no CSV, no API. ORCA (Seattle): similar web UI, no API. Oyster (London): account at oyster.tfl.gov.uk shows 8 weeks of journey history; no structured export. Most transit card operators offer no machine-readable export for riders.
- **Recommendation:** Icebox — PDF-only exports are painful to parse and not reliable across card operators. Revisit if any major transit operator introduces CSV/JSON export. For now, Apple Maps or Google Maps trip history covers commute patterns more reliably.
- **Notes:** Clipper Card in Bay Area moved to Clipper 2.0 in December 2025 (tap-to-pay credit cards now accepted) but still no API for riders. Oyster Card (London) offers 8-week journey history web view only. ORCA (Seattle), Ventra (Chicago), CharlieCard (Boston) — similar limitations. Transit app (transitapp.com) provides real-time transit APIs for agencies but has no personal journey history. The most pragmatic path is if the user's bank/credit card records transit charges — the existing bank CSV import would capture fare payments.

#### Google Maps Visited Places (Apple Maps analog) — _Place Visits_

🟢 **High — Takeout still exports Saved Places (distinct from Timeline which moved on-device). GeoJSON format is easy to parse.** · M1 · none (Google Takeout export, requires Google account) · effort **S** · 🆕 new

- **Access:** Google Maps on iOS/Android maintains a local 'Your Places' saved list (not the same as Timeline). Google Takeout includes Saved Places in a separate JSON file — available via takeout.google.com → Maps (your places). Format: GeoJSON FeatureCollection with place name, address, coordinates, optional URL.
- **Recommendation:** Build later — add as part of the Google Timeline import. Saved Places are user's starred/saved locations (not visited history), but still useful as a places-of-interest layer.
- **Notes:** Takeout → Google Maps → 'Saved Places' is different from Timeline/Location History. Saved Places JSON is GeoJSON with arrays: Starred places, Labeled places (Home/Work), Want to go, etc. This is user-curated favorites, not visit history — lower priority than Timeline raw GPS.

### Geolocation & Travel — cross-cutting notes

1. ENCRYPTION WALL — Apple's entire Significant Locations ecosystem (routined cache_encryptedA/B.db, Maps Visited Places) is end-to-end encrypted with Secure Enclave device keys. Full Disk Access does not help. The only path to this data is a companion iOS app using official CoreLocation APIs to read the user's own location live — an M4 mechanism that requires an iOS app, which is outside Trove's current macOS scope. Do not spend engineering time on these paths without an iOS companion app story.\n\n2. GPS TRAIL CONSOLIDATION — Multiple sources (Strava, Garmin, Apple Health workout routes, Overland/OwnTracks, Google Timeline) all produce GPS tracks. Trove needs a canonical `location/` trails format before building more than one (workout-embedded routes stay whole in `health/`; see taxonomy). Recommend: JSONL with {timestamp, lat, lon, ele, accuracy, source} per point, plus a JSONL index of track metadata (start, end, activity_type, source, file_ref). GPX is the interchange format for import/export but JSONL is the vault format for analysis.\n\n3. FLIGHT DATA CONSOLIDATION — Three sources cover flights with increasing coverage: (a) Flighty macOS SQLite M3 (zero-config for Flighty users), (b) myFlightRadar24 CSV M1, (c) flight confirmation email parsing M1. All three should write to the canonical `travel/` trip-segment JSONL. The email parser should be a post-processor over the already-built email import, not a new collection path.\n\n4. STRAVA AS GPS AGGREGATOR — Many users with Garmin, Wahoo, or Polar devices auto-sync to Strava. Building Strava M5 first covers the majority of fitness GPS users without needing device-specific integrations. Garmin, Wahoo, and Polar direct integrations then become incremental additions for users who don't use Strava.\n\n5. OAUTH CREDENTIAL MODEL — Strava, Wahoo, Smartcar, and Swarm all use OAuth 2.0 (self-serve app registration). Tesla uses OAuth 2.0 but requires app registration review. The existing OAuth infrastructure in crates/trove-core/src/sync/ (built for Oura + TickTick) is directly reusable. Register compiled-in app credentials for Strava, Wahoo, Swarm — these are public consumer apps with standard per-user OAuth grants.\n\n6. DEAD / CLOSING SOURCES — Automatic (OBD, dead 2020), TripIt API (closed to new apps 2024), Foursquare City Guide (dead Dec 2024). Do not invest in these. Swarm is alive but uncertain; treat as low priority.\n\n7. BUILD ORDER RECOMMENDATION — (1) Apple Health workout GPX extraction (zero new mechanism, S effort, data already in vault); (2) Strava M5 (highest value GPS + activity source, M effort, clean API); (3) Google Timeline M1 parser (rich semantic data, M effort); (4) Flighty macOS SQLite M3 (S effort, confirmed path); (5) myFlightRadar24 CSV M1 (S effort); (6) Garmin export M1 (S effort, common user base); then later: Wahoo, Swarm, flight email parser, Overland/OwnTracks receiver, AwardWallet."

---

## Calendar, Tasks, Habits & Productivity

This domain covers time and intent tracking across calendar services, task managers, habit trackers, and time-tracking tools. Apple Calendar and Reminders (EventKit) plus TickTick are already built. The domain splits cleanly into three tiers: (1) local-store apps like Things 3 and Toggl Track that expose readable SQLite databases on disk — highest feasibility, no network required; (2) well-documented cloud APIs (Google Calendar, Todoist, Toggl Track, Linear, Asana, Jira, Trello, Microsoft/Google Tasks, Habitica, Harvest, Clockify, Calendly, Motion) that accept personal API keys or standard OAuth and pull the user's own data; (3) niche/opaque cases (OmniFocus proprietary ZIP-XML format, Sunsama gated API, Streaks with no public API) that require workarounds or export-only. Overall feasibility is high: most major services have stable, documented APIs or readable local stores that align cleanly with Trove's local-first, standalone constraints.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Things 3 | Task Manager | M3 | FDA (file lives in a sandboxed Group Container that requires Full Disk Access to read from another process) | S | 🟢 High — standard SQLite, well-documented community schema, contains every task field; app does not need to be running | 🆕 new |
| Google Calendar | Calendar | M5 | OAuth (Google account sign-in, user grants calendar read scope) | S | 🟢 High — stable v3 API since 2011, CalDAV also supported, full event history accessible | 🆕 new |
| Todoist | Task Manager | M5 | OAuth or personal API token (Settings > Integrations > Developer in Todoist web app) | S | 🟢 High — v1 API (unified REST+Sync) is current and stable; completed tasks accessible via /tasks/completed_by_completion_date | 🆕 new |
| Toggl Track | Time Tracking | M3 | FDA for local DB; API token (no OAuth needed — user copies token from Toggl web profile page) | S | 🟢 High — local SQLite has ZMANAGEDTIMEENTRY table with full entry history; API v9 is stable and well documented | 🆕 new |
| Microsoft Outlook Calendar | Calendar | M5 | OAuth (Microsoft account + Azure AD app registration; delegated Calendars.Read scope) | M | 🟢 High — Graph API is stable, well documented, and handles both personal Microsoft accounts and M365 accounts | 🆕 new |
| Google Tasks | Task Manager | M5 | OAuth (Google account, tasks.readonly scope) | S | 🟢 High — free API, stable, covers all task lists and tasks with due dates and completion status | 🆕 new |
| Microsoft To Do | Task Manager | M5 | OAuth (Microsoft account + Azure AD app; delegated Tasks.Read) | M | 🟢 High — stable v1.0 Graph endpoint, rich task model including due date, reminder, recurrence, and checklist items | 🆕 new |
| Calendly | Scheduling / Meeting History | M5 | Personal access token (Calendly Settings > Integrations > API & Webhooks) or OAuth 2.1 | S | 🟢 High — stable v2 API, personal token requires no app registration, event history fully accessible | 🆕 new |
| Linear | Task Manager / Engineering | M5 | Personal API key (no OAuth required for personal use; OAuth available for multi-user apps) | S | 🟢 High — stable GraphQL API, personal key requires no app registration, full issue history with all fields | 🆕 new |
| Habitica | Habit Tracker / Gamified Tasks | M5 | API key (user finds User ID + API Token in Habitica Settings > API; no app registration) | S | 🟢 High — v3 API is the only supported version (v1/v2 shut down), stable, full task and habit data accessible | 🆕 new |
| Jira (Cloud) | Task Manager / Project Management | M5 | API token (Atlassian account; no OAuth app registration for personal use — Basic Auth with email + token) | M | 🟢 High — stable REST API, personal API token allows read of all assigned/created issues | 🆕 new |
| Asana | Task Manager / Project Management | M5 | Personal access token (Asana Developer Console > My Apps > Personal Access Token; no app registration needed) | S | 🟢 High — stable REST API, personal token is frictionless, full task history and project membership | 🆕 new |
| Trello | Task Manager / Kanban | M5 | API key + user token (Trello Developer Portal; OAuth 1.0 flow for token; key is per-app, free) | S | 🟢 High — stable REST API, comprehensive data model, full card/board/list history | 🆕 new |
| Clockify | Time Tracking | M5 | API key (Clockify Profile Settings > API; no OAuth needed for personal use) | S | 🟢 High — stable v1 API, API key requires no registration, full time entry history | 🆕 new |
| Harvest (Time Tracking) | Time Tracking | M5 | Personal Access Token from id.getharvest.com/developers (no OAuth app registration needed for personal use) | S | 🟢 High — stable v2 API, personal token requires no app registration, full time entry history | 🆕 new |
| Motion (AI Scheduler) | Task Manager / AI Scheduling | M5 | API key (Motion Settings > API; requires paid plan) | S | 🟡 Medium — official REST API exists and covers tasks/projects/schedules; requires paid subscription to access | 🆕 new |
| CalDAV (Generic) | Calendar / Protocol | M5 | Username + password or app-specific password depending on provider | M | 🟢 High — open standard; Rust crate `ical` parses .ics; covers any CalDAV-compliant calendar including self-hosted | 🆕 new |
| Cal.com | Scheduling / Meeting History | M5 | API key (Cal.com Settings; free on hosted plan; self-hosted has direct DB access via PostgreSQL) | S | 🟢 High — open-source, stable v2 API, full booking history including cancelled events | 🆕 new |
| Notion (Tasks/Databases) | Task Manager / Notes | M5 | Integration token (Notion Settings > Connections; user must share specific pages/databases with the integration) | M | 🟡 Medium — stable API but requires per-database sharing; no bulk workspace dump; 3 req/s rate limit; JSON block structure needs translation | 🆕 new |
| OmniFocus | Task Manager | M3 | FDA (sandboxed container) | L | 🟡 Medium — data is on disk and parseable but NOT standard SQLite; requires implementing a custom ZIP+XML parser for the .ofocus transaction log format | 🆕 new |
| Amazing Marvin | Task Manager | M5 | API key (Amazing Marvin Settings > API; requires active subscription) | S | 🟡 Medium — API exists and is documented, covers tasks/projects/habits/dailies; gated behind subscription | 🆕 new |
| Timing (Automatic Time Tracker) | Time Tracking | M3 | FDA (sandboxed app support path) | M | 🟡 Medium — local SQLite is readable but schema is not officially documented; community reverse-engineering exists (timingapp-ruby gem documents tables) | 🆕 new |
| Sunsama | Daily Planner | M5 | Subscription-gated API key | L | 🟠 Low — no public API documentation; API locked behind expensive subscription tier; no reliable export path for free/standard users | 🆕 new |
| Habitify (Habit Tracker) | Habit Tracker | M5 | API key (Habitify app Settings; requires Pro subscription) | S | 🟡 Medium — documented REST API, covers habits, journal entries, and completion logs; Pro subscription required | 🆕 new |
| Way of Life (Habit Tracker) | Habit Tracker | M1 | none — user-initiated export from app | S | 🟡 Medium — export exists, format is CSV/Excel, but no API means no automated pulls; user must initiate manually | 🆕 new |
| Streaks (Habit Tracker) | Habit Tracker | M1 | none (if export exists) — iOS app only | M | 🟠 Low — no API, no documented export, iCloud sync is opaque; Catalyst Mac build may have iCloud container accessible but schema unknown | 🆕 new |
| Fantastical | Calendar / Tasks | M4 | TCC-Calendar (already granted for Apple Calendar integration) | S | 🟢 High — no separate integration needed; Fantastical events appear in Apple Calendar/EventKit which is already built | 🆕 new |
| Org-mode / Plain-text task files | Task Manager / Plain Text | M2 | none (user's own file directory) | M | 🟡 Medium — .org format is well-specified plain text; Rust parsing is straightforward; covers Emacs org-mode, beorg (iCloud-synced), Logseq (org backend), and Doom/Spacemacs users | 🆕 new |
| Timery | Time Tracking (Toggl frontend) | M5 | same as Toggl Track | S | 🟢 High — covered entirely by Toggl Track integration | 🆕 new |

### Detail

#### Things 3 — _Task Manager_

🟢 **High — standard SQLite, well-documented community schema, contains every task field; app does not need to be running** · M3 · FDA (file lives in a sandboxed Group Container that requires Full Disk Access to read from another process) · effort **S** · 🆕 new

- **Access:** Local SQLite at ~/Library/Group Containers/JLMPQHK86H.com.culturedcode.ThingsMac/ThingsData-*/Things Database.thingsdatabase/main.sqlite — standard sqlite3 read
- **Recommendation:** Build now — highest-value local task manager on Mac; fast M3 poll, rich schema
- **Notes:** Things must be quit before writing to the DB (reads are safe while running). Community libraries things.py and things.sh both work against main.sqlite. Schema has ZTASK, ZAREA, ZPROJECT, ZCHECKLISTITEM, ZLOGITEM tables. Beta build uses com.culturedcode.ThingsMac.beta path. No official export API exists; SQLite is the only programmatic route. Things URL scheme is write-only so cannot be used to pull data.

#### Google Calendar — _Calendar_

🟢 **High — stable v3 API since 2011, CalDAV also supported, full event history accessible** · M5 · OAuth (Google account sign-in, user grants calendar read scope) · effort **S** · 🆕 new

- **Access:** Google Calendar API v3 — GET https://www.googleapis.com/calendar/v3/calendars/primary/events; OAuth 2.0 scope https://www.googleapis.com/auth/calendar.readonly
- **Recommendation:** Build now — extremely common calendar, pairs naturally with Apple Calendar already built
- **Notes:** New writerWithoutPrivateAccess access level rolls out June 29 2026 but doesn't affect read-only pulls. CalDAV at https://www.googleapis.com/caldav/v2/calendars is an alternative M5 path using iCalendar format. Rate limits: 1M queries/day free tier. Pagination via nextPageToken. All calendars listable via /users/me/calendarList.

#### Todoist — _Task Manager_

🟢 **High — v1 API (unified REST+Sync) is current and stable; completed tasks accessible via /tasks/completed_by_completion_date** · M5 · OAuth or personal API token (Settings > Integrations > Developer in Todoist web app) · effort **S** · 🆕 new

- **Access:** Todoist REST API v1 — GET https://api.todoist.com/api/v1/tasks, POST /sync with resource_types=["all"] and sync_token=* for full dump; Bearer token auth
- **Recommendation:** Build now — very large user base, clean API, completed tasks history available
- **Notes:** REST v2 is deprecated; use v1 which unifies REST and Sync APIs. Sync endpoint with resource_types=["all"] pulls projects, sections, labels, tasks, reminders, notes, filters in one call — ideal for initial import. No rate limit listed publicly; practical limit ~50 req/s. Completed tasks require a paid Todoist plan to access beyond 1 week.

#### Toggl Track — _Time Tracking_

🟢 **High — local SQLite has ZMANAGEDTIMEENTRY table with full entry history; API v9 is stable and well documented** · M3 · FDA for local DB; API token (no OAuth needed — user copies token from Toggl web profile page) · effort **S** · 🆕 new

- **Access:** Two paths: (1) M3 local CoreData SQLite at ~/Library/Group Containers/B227VTMZ94.group.com.toggl.daneel.extensions/production/DatabaseModel.sqlite; (2) M5 API GET https://api.track.toggl.com/api/v9/me/time_entries — API token as Basic Auth password
- **Recommendation:** Build now — dual-path (local DB for speed, API for cross-device history); one of the most popular time trackers
- **Notes:** Local DB is CoreData-backed with 19 entity tables. ZMANAGEDTIMEENTRY contains start, stop, description, project FK. macOS 26 introduced a transient issue where the DB file couldn't be opened after an OS upgrade; Toggl fixed in v10.15.0 — monitor for regressions. API v9 rate limit: 30 req/hour for /me endpoints. API token auth is simpler than OAuth for personal use.

#### Microsoft Outlook Calendar — _Calendar_

🟢 **High — Graph API is stable, well documented, and handles both personal Microsoft accounts and M365 accounts** · M5 · OAuth (Microsoft account + Azure AD app registration; delegated Calendars.Read scope) · effort **M** · 🆕 new

- **Access:** Microsoft Graph API — GET https://graph.microsoft.com/v1.0/me/events; OAuth 2.0 via Azure AD with scope Calendars.Read
- **Recommendation:** Build now — essential for enterprise and Microsoft-ecosystem users
- **Notes:** EWS (legacy Exchange) shuts down October 2026 — Graph is the only forward path. CalDAV also works for personal Outlook.com accounts. Delta query ($deltaToken) enables efficient incremental sync. Calendars include personal + shared. M365 requires admin consent for some scopes; personal Microsoft accounts use standard OAuth consent flow. Need to register an Azure app (free) for OAuth credentials.

#### Google Tasks — _Task Manager_

🟢 **High — free API, stable, covers all task lists and tasks with due dates and completion status** · M5 · OAuth (Google account, tasks.readonly scope) · effort **S** · 🆕 new

- **Access:** Google Tasks API v1 — GET https://tasks.googleapis.com/tasks/v1/lists/@default/tasks; OAuth scope https://www.googleapis.com/auth/tasks.readonly
- **Recommendation:** Build now — lightweight, often bundled with Google Calendar integration
- **Notes:** API is free with standard Google Cloud quotas. Resources: task lists (GET /lists) and tasks (GET /lists/{taskList}/tasks). Supports completed tasks. Can share OAuth flow with Google Calendar integration. Limited data model (no notes/attachments in tasks), but covers the core.

#### Microsoft To Do — _Task Manager_

🟢 **High — stable v1.0 Graph endpoint, rich task model including due date, reminder, recurrence, and checklist items** · M5 · OAuth (Microsoft account + Azure AD app; delegated Tasks.Read) · effort **M** · 🆕 new

- **Access:** Microsoft Graph API — GET https://graph.microsoft.com/v1.0/me/todo/lists and /lists/{id}/tasks; OAuth scope Tasks.Read
- **Recommendation:** Build now — bundles naturally with Outlook Calendar integration; covers Wunderlist/To Do migrated users
- **Notes:** Can reuse the same Azure app registration as Outlook Calendar. Task model includes bodyContent (rich text notes), dueDateTime, completedDateTime, reminderDateTime, importance, recurrence, linkedResources. Beta endpoint /me/tasks exposes additional fields. Personal Microsoft accounts work without admin consent.

#### Calendly — _Scheduling / Meeting History_

🟢 **High — stable v2 API, personal token requires no app registration, event history fully accessible** · M5 · Personal access token (Calendly Settings > Integrations > API & Webhooks) or OAuth 2.1 · effort **S** · 🆕 new

- **Access:** Calendly API v2 — GET https://api.calendly.com/scheduled_events (personal access token or OAuth 2.1); list all events booked through the user's Calendly links
- **Recommendation:** Build now — valuable meeting-history record; minimal friction with personal access tokens
- **Notes:** GET /scheduled_events returns all meetings booked on the user's account including status (active, cancelled). GET /scheduled_events/{uuid}/invitees returns attendee details. Webhooks available for live capture. Rate limit: 100 req/min. No OAuth app registration needed for personal-use token.

#### Linear — _Task Manager / Engineering_

🟢 **High — stable GraphQL API, personal key requires no app registration, full issue history with all fields** · M5 · Personal API key (no OAuth required for personal use; OAuth available for multi-user apps) · effort **S** · 🆕 new

- **Access:** Linear GraphQL API — POST https://api.linear.app/graphql; personal API key from Settings > Account > Security & Access; query `me { assignedIssues { nodes { ... } } }`
- **Recommendation:** Build now — popular among developers/engineers; personal API key is trivial to obtain
- **Notes:** Personal API key scopes: Read, Write, Admin, Create issues, Create comments — read-only is sufficient. Rate limit ~100-300 req/min per token. Pagination via cursor (pageInfo.endCursor / after). Can pull issues, comments, cycles, projects, labels, attachments. Team/workspace context required for some queries.

#### Habitica — _Habit Tracker / Gamified Tasks_

🟢 **High — v3 API is the only supported version (v1/v2 shut down), stable, full task and habit data accessible** · M5 · API key (user finds User ID + API Token in Habitica Settings > API; no app registration) · effort **S** · 🆕 new

- **Access:** Habitica API v3 — GET https://habitica.com/api/v3/user (full user data), GET /tasks/user?type=habits|dailys|todos; auth headers x-api-user + x-api-key + x-client
- **Recommendation:** Build now — unique gamified habit/task tracker data; API keys are free and require no registration
- **Notes:** x-client header is mandatory as of late 2025 — must be set to a unique app identifier. GET /user returns all user data including XP, streaks, and task history, exportable as JSON. Historical data note: Habitica averages older task history; detailed checkin logs not preserved indefinitely. GET /user/export/userdata.json is the bulk export endpoint.

#### Jira (Cloud) — _Task Manager / Project Management_

🟢 **High — stable REST API, personal API token allows read of all assigned/created issues** · M5 · API token (Atlassian account; no OAuth app registration for personal use — Basic Auth with email + token) · effort **M** · 🆕 new

- **Access:** Jira Cloud REST API v3 — GET https://{domain}.atlassian.net/rest/api/3/search?jql=assignee=currentUser(); API token from Atlassian Account Settings > Security > API tokens
- **Recommendation:** Build now — widely used in engineering teams; API token auth is low friction
- **Notes:** JQL query assignee=currentUser() or reporter=currentUser() pulls personal issues. Full field set returned by default (use fields= to limit). Pagination via startAt/maxResults. Rate limits apply per user. Jira Server/Data Center uses a different base URL and older API version — target Cloud first.

#### Asana — _Task Manager / Project Management_

🟢 **High — stable REST API, personal token is frictionless, full task history and project membership** · M5 · Personal access token (Asana Developer Console > My Apps > Personal Access Token; no app registration needed) · effort **S** · 🆕 new

- **Access:** Asana REST API — GET https://app.asana.com/api/1.0/tasks?assignee=me&workspace={workspace_gid}; OAuth 2.0 or Personal Access Token from Asana Developer Console
- **Recommendation:** Build now — common in professional teams; PAT is instant to obtain
- **Notes:** GET /tasks?assignee=me pulls all assigned tasks across workspaces. Workspace GIDs needed from GET /workspaces. Supports opt_fields to request specific fields. Rate limit: 150 req/min per app/user combo. Completed tasks retrievable with completed=true. Projects, sections, tags, followers all included.

#### Trello — _Task Manager / Kanban_

🟢 **High — stable REST API, comprehensive data model, full card/board/list history** · M5 · API key + user token (Trello Developer Portal; OAuth 1.0 flow for token; key is per-app, free) · effort **S** · 🆕 new

- **Access:** Trello REST API — GET https://api.trello.com/1/members/me/boards?key={key}&token={token}; then GET /boards/{id}/cards; API key + token from developer.atlassian.com/app
- **Recommendation:** Build now — large user base, simple key+token auth, full JSON export of boards
- **Notes:** GET /boards/{id}?fields=all&cards=all&lists=all returns a full board snapshot. JSON export also available via board menu. Cards include due dates, checklists, labels, attachments, comments. Rate limit: 100 req/10s per token, 300 req/10s per key. Power-Ups (calendar, timeline) accessible via same API.

#### Clockify — _Time Tracking_

🟢 **High — stable v1 API, API key requires no registration, full time entry history** · M5 · API key (Clockify Profile Settings > API; no OAuth needed for personal use) · effort **S** · 🆕 new

- **Access:** Clockify REST API — GET https://api.clockify.me/api/v1/workspaces/{workspaceId}/user/{userId}/time-entries; API key header X-Api-Key from Profile Settings
- **Recommendation:** Build now — free plan has no API limit; very popular team/freelance time tracker
- **Notes:** GET /user returns current user ID and workspace ID. Rate limit: 10 req/s. Time entries include project, task, tags, description, start/end. Pagination via page/page-size. Free tier users have full API access. Reports API also available for aggregated data.

#### Harvest (Time Tracking) — _Time Tracking_

🟢 **High — stable v2 API, personal token requires no app registration, full time entry history** · M5 · Personal Access Token from id.getharvest.com/developers (no OAuth app registration needed for personal use) · effort **S** · 🆕 new

- **Access:** Harvest API v2 — GET https://api.harvestapp.com/v2/time_entries; Bearer token + Harvest-Account-Id header; Personal Access Token from Harvest ID developer settings
- **Recommendation:** Build now — popular among freelancers; token-based auth is instant
- **Notes:** Members can only access their own tracked time via the API. Requires both the token AND Harvest-Account-Id header. Entries include project, task, client, hours, notes, spent_date. Pagination via next/prev links. Note: a separate Greenhouse product also called 'Harvest' is a recruiting API — the time-tracking Harvest is at harvestapp.com. v2 is current; an unrelated Greenhouse v3 migration warning does not affect Harvest time tracking.

#### Motion (AI Scheduler) — _Task Manager / AI Scheduling_

🟡 **Medium — official REST API exists and covers tasks/projects/schedules; requires paid subscription to access** · M5 · API key (Motion Settings > API; requires paid plan) · effort **S** · 🆕 new

- **Access:** Motion REST API — GET https://api.usemotion.com/v1/tasks and /v1/schedules; API key from Motion Settings > API Keys
- **Recommendation:** Build later — growing user base; gated behind paid plan limits broad deployment
- **Notes:** API supports list/create/update/delete for tasks, projects, users. Schedule endpoint returns auto-scheduled task slots. Appointment scheduling feature NOT available via API. Unofficial MCP server (RF-D/motion-mcp on GitHub) exists as reference for API shape. Data export available before cancellation.

#### CalDAV (Generic) — _Calendar / Protocol_

🟢 **High — open standard; Rust crate `ical` parses .ics; covers any CalDAV-compliant calendar including self-hosted** · M5 · Username + password or app-specific password depending on provider · effort **M** · 🆕 new

- **Access:** CalDAV RFC 4791 — HTTP REPORT requests to user's CalDAV server URL; iCalendar (.ics) format; servers include iCloud (caldav.icloud.com), Google (calendar.google.com/caldav/v2), Fastmail, Nextcloud, self-hosted Radicale/Baikal
- **Recommendation:** Build now — catch-all for non-Google/non-Microsoft calendars; self-hosters will love it
- **Notes:** CalDAV is the protocol under iCloud Calendar, Google Calendar (alternative path), Fastmail, Proton Calendar (beta), and all self-hosted solutions. PROPFIND/REPORT verbs retrieve calendar objects. Rust library `caldav-client` or `mini-dav` can be compiled in. iCloud requires an app-specific password (2FA accounts). Proton Calendar added CalDAV support in 2024 via Bridge — feasible as M5.

#### Cal.com — _Scheduling / Meeting History_

🟢 **High — open-source, stable v2 API, full booking history including cancelled events** · M5 · API key (Cal.com Settings; free on hosted plan; self-hosted has direct DB access via PostgreSQL) · effort **S** · 🆕 new

- **Access:** Cal.com REST API v2 — GET https://api.cal.com/v2/bookings; Bearer API key from Cal.com Settings > Developer > API keys; self-hosted instances expose same API at custom URL
- **Recommendation:** Build now — growing Calendly alternative; API key is instant; self-hosted variant is a bonus
- **Notes:** Cal.diy (self-hosted fork) moved closed-source in 2025 but hosted Cal.com remains open. Bookings include attendee, event type, start/end, status. Self-hosted users can query PostgreSQL directly (M3/M6). Rate limit documentation sparse but API is free tier accessible.

#### Notion (Tasks/Databases) — _Task Manager / Notes_

🟡 **Medium — stable API but requires per-database sharing; no bulk workspace dump; 3 req/s rate limit; JSON block structure needs translation** · M5 · Integration token (Notion Settings > Connections; user must share specific pages/databases with the integration) · effort **M** · 🆕 new

- **Access:** Notion REST API — POST https://api.notion.com/v1/databases/{database_id}/query; Personal Access Token from Notion Settings > Connections > Develop or create integrations
- **Recommendation:** Build later — very popular but auth friction (per-page sharing), rate limit, and no native export endpoint make full extraction slow
- **Notes:** New in May 2026: Markdown API for reading/writing pages as Markdown (built for AI agents). POST /databases/{id}/query with filter by checkbox/status to get task items. No single /export endpoint — must traverse page tree. PATs are user-scoped. Rate limit: 3 req/s. Manual export (Settings > Export) produces Markdown+CSV zip — M1 import is a pragmatic fallback.

#### OmniFocus — _Task Manager_

🟡 **Medium — data is on disk and parseable but NOT standard SQLite; requires implementing a custom ZIP+XML parser for the .ofocus transaction log format** · M3 · FDA (sandboxed container) · effort **L** · 🆕 new

- **Access:** Proprietary .ofocus database format at ~/Library/Containers/com.omnigroup.OmniFocus4/Data/Library/Application Support/OmniFocus/OmniFocus.ofocus — ZIP bundles of XML transactions. Requires parsing. Alternative: Omni Automation JS API (requires app running).
- **Recommendation:** Spike first — worth building for OmniFocus power users but non-trivial; community parsers (tomzx/ofocus-format on GitHub) provide the schema spec
- **Notes:** .ofocus is a directory of ZIP files each containing contents.xml with transaction deltas. Must replay all transactions to reconstruct current state. No stable schema guarantee. Omni Automation JavaScript API requires the app to be running (violates standalone rule for M6). OmniFocus offers no REST API. Export to TaskPaper/JSON via File > Export is M1 fallback.

#### Amazing Marvin — _Task Manager_

🟡 **Medium — API exists and is documented, covers tasks/projects/habits/dailies; gated behind subscription** · M5 · API key (Amazing Marvin Settings > API; requires active subscription) · effort **S** · 🆕 new

- **Access:** Amazing Marvin REST API — GET https://serv.amazingmarvin.com/api/todayItems, /api/tasks, /api/habits; API key from Amazing Marvin Settings > API
- **Recommendation:** Build later — niche but enthusiastic ADHD/productivity power user base; API works but subscription requirement limits audience
- **Notes:** MCP server (bgheneti/Amazing-Marvin-MCP on GitHub) provides 28 tools including get_daily_productivity_overview and get_all_tasks — good reference for API surface. Habits have streak data and check-in history. GET /api/habits returns habit definitions; scoring/checkin endpoints separate.

#### Timing (Automatic Time Tracker) — _Time Tracking_

🟡 **Medium — local SQLite is readable but schema is not officially documented; community reverse-engineering exists (timingapp-ruby gem documents tables)** · M3 · FDA (sandboxed app support path) · effort **M** · 🆕 new

- **Access:** Two paths: (1) M3 local SQLite at ~/Library/Application Support/info.eurocomp.Timing2/SQLite.db — full activity/time entry data; (2) JavaScript scripting API (requires Timing Connect subscription) for export
- **Recommendation:** Spike first — local DB approach avoids subscription requirement; schema reverse-engineering needed; niche but loyal macOS power user base
- **Notes:** Timing tracks app usage automatically (like Screen Time but more detailed with project categorization). SQLite contains activity and time entry tables. Community Ruby gem `timingapp` documents the schema. JavaScript scripting via Timing Connect is the official path but requires paid plan. Export via Reports (CSV/JSON) is M1 fallback.

#### Sunsama — _Daily Planner_

🟠 **Low — no public API documentation; API locked behind expensive subscription tier; no reliable export path for free/standard users** · M5 · Subscription-gated API key · effort **L** · 🆕 new

- **Access:** API access only available on Power Pro plan ($65/month). No documented public API endpoints found. MCP access listed as a Power Pro feature. Manual export reportedly difficult — no CSV/JSON export.
- **Recommendation:** Icebox — API access exists in principle but is undocumented, expensive, and would serve a small subset of users
- **Notes:** Sunsama integrates Todoist, Asana, Linear, Jira, etc. — capturing data from those upstream sources is a better approach. Community feature request for a free API has been open for years. Data export is reportedly near-impossible (manual copy-paste).

#### Habitify (Habit Tracker) — _Habit Tracker_

🟡 **Medium — documented REST API, covers habits, journal entries, and completion logs; Pro subscription required** · M5 · API key (Habitify app Settings; requires Pro subscription) · effort **S** · 🆕 new

- **Access:** Habitify REST API — GET https://api.habitify.me/habits and /journal; API key from Habitify Settings > API Access
- **Recommendation:** Build later — smaller user base than Habitica; Pro gate limits reach, but API is clean and well-documented
- **Notes:** GET /habits returns all habits with streak data. GET /journal?habit_id={id} returns completion log. Date-filtered queries supported. iCloud-based sync means no local DB on macOS. Official API docs at docs.habitify.me.

#### Way of Life (Habit Tracker) — _Habit Tracker_

🟡 **Medium — export exists, format is CSV/Excel, but no API means no automated pulls; user must initiate manually** · M1 · none — user-initiated export from app · effort **S** · 🆕 new

- **Access:** Data export: CSV and Excel from within the app (Settings > Export). No public REST API documented. URL-scheme for cross-app interaction only.
- **Recommendation:** Build later — M1 CSV import covers the use case; no API means no automation; niche app
- **Notes:** iOS/Android only (no Mac app). Export formats: CSV and Excel. Data includes habit names, dates, yes/no/skip values. Android also supports JSON export. Last updated August 2025. No API; no webhooks. M1 import is the only feasible integration path.

#### Streaks (Habit Tracker) — _Habit Tracker_

🟠 **Low — no API, no documented export, iCloud sync is opaque; Catalyst Mac build may have iCloud container accessible but schema unknown** · M1 · none (if export exists) — iOS app only · effort **M** · 🆕 new

- **Access:** No public API. Data syncs via iCloud. No documented local macOS DB path. The app is iOS-primary with a Catalyst Mac build.
- **Recommendation:** Icebox — no access path beyond manual screenshots; iCloud container schema not publicly documented
- **Notes:** Streaks syncs habits via iCloud. A 'Streaks 2026' variant appeared on the App Store (separate app, id 6740426283) — likely unrelated. Original Streaks by Crunchy Bagel (id 963034692) is the mainstream app. No export feature advertised. HealthKit integration means workout-linked habits might appear in Apple Health export.

#### Fantastical — _Calendar / Tasks_

🟢 **High — no separate integration needed; Fantastical events appear in Apple Calendar/EventKit which is already built** · M4 · TCC-Calendar (already granted for Apple Calendar integration) · effort **S** · 🆕 new

- **Access:** Fantastical stores calendar data via macOS Calendar/EventKit (Apple Calendar backend) or direct CalDAV/Exchange accounts — no proprietary local DB. The underlying data is accessible via EventKit (already built) or CalDAV. Fantastical Reminders sync through Apple Reminders.
- **Recommendation:** Skip — covered by Apple Calendar (EventKit, already built); Fantastical is a frontend over the same stores
- **Notes:** Fantastical 3+ does NOT use macOS Calendar APIs by default — it manages its own CalDAV connections. However, events Fantastical manages are still reflected in Apple Calendar if accounts are added at the macOS level. Recommends noting this in Apple Calendar docs rather than building a separate Fantastical connector. Fantastical tasks sync to Apple Reminders.

#### Org-mode / Plain-text task files — _Task Manager / Plain Text_

🟡 **Medium — .org format is well-specified plain text; Rust parsing is straightforward; covers Emacs org-mode, beorg (iCloud-synced), Logseq (org backend), and Doom/Spacemacs users** · M2 · none (user's own file directory) · effort **M** · 🆕 new

- **Access:** M2 watch folder — Trove watches a user-configured directory (e.g. ~/org, ~/Documents/notes) for .org files; parse TODO keywords, SCHEDULED, DEADLINE, DONE timestamps via text parsing
- **Recommendation:** Build later — niche but dedicated user base (developers/Emacs users); M2 watch folder is clean and respects files-as-truth principle
- **Notes:** Org format spec at orgmode.org. TODO keywords, priority cookies, tags, timestamps, and LOGBOOK drawers all parseable. beorg syncs .org files via iCloud to ~/Library/Mobile Documents/iCloud~com~appsonthemove~beorg/Documents/. Logseq optionally uses org format. No Rust org-mode crate is mature yet — may need to write a minimal parser or use org-rs.

#### Timery — _Time Tracking (Toggl frontend)_

🟢 **High — covered entirely by Toggl Track integration** · M5 · same as Toggl Track · effort **S** · 🆕 new

- **Access:** Timery is a frontend for Toggl Track — all data lives in Toggl. Use Toggl Track API (M5) or Toggl local SQLite (M3) instead. No separate data store.
- **Recommendation:** Skip — Timery has no independent data store; covered by Toggl Track connector
- **Notes:** Timery caches Toggl data in iCloud for cross-device sync but the authoritative store is Toggl. The Toggl API or local Toggl SQLite is the right access path.

### Calendar, Tasks, Habits & Productivity — cross-cutting notes

1. LOCAL DB PATTERN (Things 3, Toggl Track, Timing): All three use app-sandboxed containers requiring Full Disk Access. The poll pattern (copy-then-read, respecting app's own lock behavior) is identical. A shared M3 collector trait in trove-core that handles FDA path resolution, file-locking detection, and SQLite reads will cover all three. Things 3 and Toggl Track have well-documented community schemas; Timing requires a spike.

2. GOOGLE BUNDLE OPPORTUNITY: Google Calendar, Google Tasks, and potentially Google Drive all share the same OAuth client and token. A single OAuth flow can request all Google scopes simultaneously (calendar.readonly + tasks.readonly), eliminating redundant consent prompts. The Google integration worktree already has OAuth plumbing done — these should plug in cheaply.

3. MICROSOFT BUNDLE OPPORTUNITY: Outlook Calendar and Microsoft To Do share the same Azure AD app registration and OAuth token (Calendars.Read + Tasks.Read in one consent). One auth flow covers both. Can potentially extend to OneDrive/SharePoint later.

4. CALDAV AS CATCH-ALL: CalDAV covers iCloud Calendar (already in EventKit), Google Calendar (alternative path), Fastmail, Proton Calendar, Nextcloud, and all self-hosted solutions. A generic CalDAV M5 collector would let power users point Trove at any CalDAV server. RFC 4791 is stable. Rust crates exist (caldav-client, ical). This is the highest-leverage single connector for calendar coverage breadth.

5. API KEY vs OAUTH SPLIT: Todoist, Toggl, Clockify, Habitica, Calendly, Linear, Asana, and Harvest all support personal API keys with no app registration — users copy a token from their account settings page. This is far lower friction than full OAuth for personal-use Trove. Trove should prefer API-key flows for personal tools and reserve OAuth for services that require it (Google, Microsoft). The vault's secrets store (likely macOS Keychain via Tauri) should accept both patterns.

6. COMPLETED TASK HISTORY GAP: Several services gate completed/historical data: Todoist requires paid plan for >1 week of completed tasks; Habitica averages and discards old history. Trove should document this clearly in UI and encourage users to connect early (before history is lost) rather than retroactively.

7. SUBSCRIPTION GATES: Harvest, Motion, Habitify, and Amazing Marvin all require paid subscriptions for API access. Sunsama's API is locked behind a $65/month Power Pro tier. These should be noted prominently in the connector UI so users aren't surprised. Clockify and Google Tasks are fully free.

8. THINGS 3 vs TICKTICK COEXISTENCE: Some users run both. Things 3 is M3 (local) while TickTick is M5 (built). They should both be collectable independently, with task deduplication handled at the vault level (different source tags) rather than at collection time.

---

## Artifacts: Notes, Documents, Drafts & Files

This domain covers user-authored knowledge and documents: notes apps with local DBs, cloud-native note services, journaling apps, cloud-drive folders, and adjacent stores (password-manager metadata, e-signed docs, code snippets). The split is roughly: (a) local-first apps (Apple Notes, Bear, Drafts, Obsidian, Logseq, Ulysses, Day One) that store data in known on-disk paths — high feasibility via M3 or plain folder watch; (b) cloud-primary apps (Notion, Roam, Evernote, Google Keep, OneNote, Standard Notes, Simplenote, Reflect, Capacities, Craft) where a public API or an official export is the access path; and (c) cloud-drive folders (iCloud Drive, Dropbox, Google Drive for Desktop, OneDrive) that already appear as local folder trees on macOS. Apple Notes is planned and feasible but tricky due to gzipped-protobuf body encoding. Drafts, Obsidian, and Logseq are trivially accessible as plain files. Cloud services generally have export-or-API paths that map cleanly to M1 or M5. The main blockers are Apple Journal (brand-new Mac app, format not yet documented publicly), OneNote (API-only, delegated-auth required since March 2025), and Roam (API in beta, export-only workaround).

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Obsidian | Notes / Knowledge base | M1; M2 fallback | none — it is just the filesystem | S | 🟢 High — data is already plain markdown files in a user-visible directory; Trove reads them directly with no parsing layer needed. | 📋 planned |
| Logseq | Notes / Knowledge base | M1; M2 fallback | none | S | 🟢 High — same story as Obsidian; plain markdown on disk at a user-chosen location. | 📋 planned |
| Bear | Notes | M3 | Full Disk Access | S | 🟢 High — well-documented schema with community tooling; FDA already required by Trove for Safari, iMessage, etc. Copy-then-read pattern applies. | 📋 planned |
| Drafts | Notes / Quick capture | M3; M6 fallback via Drafts URL scheme / AppleScript | Full Disk Access | S | 🟢 High — the group container DB is documented in the community as readable by third-party tools. FDA already in Trove's grant. | 📋 planned |
| Apple Notes | Notes | M3 | Full Disk Access | M | 🟡 Medium — path is known and FDA already held; the hard part is decoding gzipped protobuf bodies. Maintained community tooling exists (apple_cloud_notes_parser in Ruby, apple-notes-parser in Python, both updated for macOS 15/16 in 2025). A Rust protobuf library (prost) can consume the same .proto definitions. | 📋 planned |
| Day One | Journaling | M1 (JSON export); M3 possible but undocumented | none for M1 export; FDA for M3 | S | 🟢 High — JSON export is official, well-structured, re-runnable. The format is stable and widely parsed by third-party tools (dayone-to-obsidian, etc.). | 🆕 new |
| iCloud Drive | Cloud Drive | M3; M2 watch | none for user-visible folder; FDA for ~/Library paths | S | 🟢 High — the local path is a standard filesystem directory. Files are real files when downloaded. The main caveat is placeholder .icloud files for un-downloaded content. | 🆕 new |
| Dropbox | Cloud Drive | M3 (local folder read); M5 fallback via API | Full Disk Access for ~/Library/CloudStorage path | S | 🟢 High — local folder is a standard filesystem location after the File Provider migration. Files are real files (not placeholders for locally-available ones). FDA already in Trove's grant. | 🆕 new |
| Google Drive for Desktop | Cloud Drive | M3 (local folder); M5 via Drive API v3 | Full Disk Access for local folder; OAuth (Google) for API | S | 🟢 High — same local-folder mechanism as Dropbox/iCloud. API is well-maintained (docs updated May 2026). | 🆕 new |
| Notion | Notes / Workspace | M1 (workspace export ZIP); M5 (API with PAT) | OAuth / API key (PAT) | M | 🟢 High — both paths are live and well-documented in 2026. PATs make personal-account M5 integration practical without full OAuth app registration. | 🆕 new |
| Capacities | Notes / Knowledge base | M1 (automated export ZIP); M5 (API) | API key / OAuth token | S | 🟢 High — automated local export means the ZIP arrives on disk without any cloud round-trip; Trove can watch the export folder for new ZIPs. API also available. | 🆕 new |
| Evernote | Notes | M1 (ENEX export); M5 possible but painful | none for M1; API key for M5 | S | 🟢 High for M1 — ENEX is stable XML and widely parsed. M5 is Low due to OAuth 1.0 + Thrift, making it unsuitable for a polished integration. | 🆕 new |
| Simplenote | Notes | M1 | none | S | 🟢 High for M1 — simplenote.json is clean JSON with all notes, tags, creation/modification dates. | 🆕 new |
| Standard Notes | Notes (encrypted) | M1 (decrypted ZIP export) | none for export | S | 🟢 High — decrypted ZIP export provides individual plain-text note files. No local DB to copy-read (always encrypted). | 🆕 new |
| Google Keep | Notes | M1 | none (Google account login for Takeout) | S | 🟢 High — Takeout is stable, JSON format is clean and well-documented in the community. | 📋 planned |
| Reflect Notes | Notes | M1 (export ZIP); M2 (daily local backup watch) | none for export | S | 🟢 High for M1/M2 — exports are standard formats; daily local backups are automatic. | 🆕 new |
| Craft | Notes / Documents | M5 (API); M1 (export) | API key (Bearer token from within app) | S | 🟢 High — Craft's own API (2025+) is the clean path; no reverse-engineering needed. | 🆕 new |
| Ulysses | Writing / Notes | M3 (iCloud path read); M1 (export Markdown) | Full Disk Access for the Library path (it is under ~/Library/Mobile Documents) | S | 🟡 Medium — iCloud path is readable with FDA; proprietary XML format needs parsing. Community tools exist (export-ulysses, ulysses-tools). Export to Markdown is the simpler M1 path. | 🆕 new |
| OneNote | Notes (Microsoft) | M5 (Graph API OAuth); M1 (DOCX/PDF export per section) | OAuth (Microsoft account) | M | 🟡 Medium — API works but requires registering an Azure AD app for OAuth2 delegated flow; page content returned as HTML (needs conversion); no bulk export endpoint. M1 export is section-by-section only (no workspace-wide dump). | 🆕 new |
| Roam Research | Notes / Knowledge base | M1 (JSON/Markdown export); M6 (MCP/CLI agent) | none for M1 | S | 🟡 Medium — export works but is manual (no programmatic trigger from outside the browser app); the backend API is in beta with limited coverage. M1 export is reliable. | 🆕 new |
| Stoic (journal app) | Journaling | M1 | none | S | 🟢 High — JSON export is structured and well-suited for import. Re-runnable with entry deduplication by UUID. | 🆕 new |
| Apple Journal | Journaling | M3 (if local container found); M1 if export feature ships | Full Disk Access (speculative) | M | 🟠 Low — app is brand-new on Mac (announced June 2025, ships fall 2026); local DB format not yet documented; no export API known. Spike needed once the app ships. | 🆕 new |
| OneDrive | Cloud Drive | M3 (local folder); M5 (Graph API) | Full Disk Access for local folder; OAuth (Microsoft) for API | S | 🟢 High — local folder is a standard filesystem path with FDA; Graph API is well-documented. | 🆕 new |
| Box | Cloud Drive | M5 (API); M3 (local folder if Box app installed) | OAuth (Box app registration); Full Disk Access for local folder | M | 🟡 Medium — API requires registering a Box developer app; personal accounts support OAuth2 only (no CCG/JWT). Niche on Mac — Box is primarily enterprise. | 🆕 new |
| Password manager metadata (1Password, Bitwarden) | Security / Metadata | M1 | none (user-initiated export, requires master password to unlock) | S | 🟢 High — both export formats are well-documented. The value for Trove is metadata only: vault item names, categories, URLs, notes, tags, creation/modified dates. Secrets (passwords) MUST NOT be imported. | 🆕 new |
| DocuSign / e-signed documents | Legal / Documents | M5 (API); M1 (manual PDF download) | OAuth (DocuSign account) | M | 🟡 Medium — API is comprehensive but requires DocuSign developer app registration. Most personal users have few documents; M1 manual download is sufficient. | 🆕 new |

### Detail

#### Obsidian — _Notes / Knowledge base_

🟢 **High — data is already plain markdown files in a user-visible directory; Trove reads them directly with no parsing layer needed.** · M1; M2 fallback · none — it is just the filesystem · effort **S** · 📋 planned

- **Access:** Plain folder of .md files at any user-chosen path. Metadata in hidden .obsidian/ subfolder (JSON). No DB, no proprietary format.
- **Recommendation:** Build now — trivially S effort; any vault path the user points at is immediately ingestable with the existing artifacts importer.
- **Notes:** User sets vault location freely (any ~/Documents subfolder is common). No sync service required. Frontmatter YAML preserved as-is. .obsidian/ config can be ignored. Just a folder-watch or M1 import.

#### Logseq — _Notes / Knowledge base_

🟢 **High — same story as Obsidian; plain markdown on disk at a user-chosen location.** · M1; M2 fallback · none · effort **S** · 📋 planned

- **Access:** User-chosen folder of .md (or .org) files. Sub-structure: pages/, journals/YYYY_MM_DD.md. Hidden .logseq/ folder holds config + backup .bak files.
- **Recommendation:** Build now — bundle with the Obsidian folder-import feature, same code path.
- **Notes:** Logseq graph location is user-set at app first-run. Journals are daily YYYY_MM_DD.md files — natural fit for Trove's day-keyed streams. Logseq had a known macOS bug where edits weren't written to disk promptly (issue #10510, 2024); advise users to confirm the setting is on-disk mode.

#### Bear — _Notes_

🟢 **High — well-documented schema with community tooling; FDA already required by Trove for Safari, iMessage, etc. Copy-then-read pattern applies.** · M3 · Full Disk Access · effort **S** · 📋 planned

- **Access:** ~/Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application Data/database.sqlite — ZSFNOTE table. Key columns: ZTITLE, ZTEXT (markdown body), ZUNIQUEIDENTIFIER, ZCREATIONDATE, ZMODIFICATIONDATE (CoreData timestamps = seconds since 2001-01-01), ZTRASHED, ZARCHIVED, ZENCRYPTED.
- **Recommendation:** Build now — S effort on existing M3 pattern. Schema is stable and thoroughly documented.
- **Notes:** ZENCRYPTED=1 rows have opaque blobs — skip or store placeholder. CoreData epoch offset: add 978307200 to get Unix time. Tags in a separate ZSFNOTETAG table joined by ZSFNOTETAG.ZNOTE. Attachments in a separate table. Bear 2 uses same container. Make a temp copy before querying (WAL may be active).

#### Drafts — _Notes / Quick capture_

🟢 **High — the group container DB is documented in the community as readable by third-party tools. FDA already in Trove's grant.** · M3; M6 fallback via Drafts URL scheme / AppleScript · Full Disk Access · effort **S** · 📋 planned

- **Access:** Primary DB: ~/Library/Group Containers/GTFQ98J4YG.com.agiletortoise.Drafts/. Contains the SQLite database with all drafts, actions, and workspaces. Also: ~/Library/Containers/com.agiletortoise.Drafts-OSX for prefs; iCloud Drive/Drafts/ for backups.
- **Recommendation:** Build now — M3 read of the group-container SQLite is the cleanest path; the M6 AppleScript fallback (noted in data-sources.md) is unnecessary once the DB path is confirmed.
- **Notes:** Schema not publicly documented by Agiletortoise; requires one-time introspection (sqlite3 .schema). App also exposes a Drafts URL scheme (drafts5://x-callback-url/...) and AppleScript dictionary for write-back if ever needed. iCloud Drive backup folder can serve as M2 watch target if FDA is ungranted. Drafts is already noted in data-sources.md as M6; upgrade to M3.

#### Apple Notes — _Notes_

🟡 **Medium — path is known and FDA already held; the hard part is decoding gzipped protobuf bodies. Maintained community tooling exists (apple_cloud_notes_parser in Ruby, apple-notes-parser in Python, both updated for macOS 15/16 in 2025). A Rust protobuf library (prost) can consume the same .proto definitions.** · M3 · Full Disk Access · effort **M** · 📋 planned

- **Access:** ~/Library/Group Containers/group.com.apple.notes/NoteStore.sqlite. Note body in ZICNOTEDATA.ZDATA column: gzip-compressed protobuf (Apple's own proto schema). Attachments in Media/<UUID>/. Locked notes are additionally encrypted.
- **Recommendation:** Build later — M effort vs. S for plain-file apps; best treated as a one-time migration importer rather than a live sync (notes don't update at high frequency). Wait until Obsidian/Bear/Drafts paths are done.
- **Notes:** Protobuf schema is reverse-engineered, not published by Apple. The schema has changed across macOS versions — the Ruby/Python parsers stay updated. Locked notes are unreadable without the Notes password. Shared notes appear in the same DB. Skip rows where ZISPASSWORDPROTECTED=1 or ZENCRYPTEDVALUEDATA is non-null. The attachment table ZICCLOUDSYNCINGOBJECT holds titles, creation dates, modification dates — useful metadata even if body parsing is skipped.

#### Day One — _Journaling_

🟢 **High — JSON export is official, well-structured, re-runnable. The format is stable and widely parsed by third-party tools (dayone-to-obsidian, etc.).** · M1 (JSON export); M3 possible but undocumented · none for M1 export; FDA for M3 · effort **S** · 🆕 new

- **Access:** Export: File > Export > JSON (.zip with Journal.json + media subfolders). JSON structure: array of entries each with uuid, creationDate, modifiedDate, text (markdown), location, weather, tags, photos/videos/audios arrays. Local DB path: ~/Library/Group Containers/5U8NS4GX82.dayoneapp2/ (not intended for direct access per Day One; schema not published). Also exports to PDF/HTML/TXT/Plain Text.
- **Recommendation:** Build now — S effort. JSON export parser handles the full schema including media references. Provide a re-importable workflow (user re-exports and drops the zip; dedup by entry UUID).
- **Notes:** Day One uses its own cloud sync (Day One Sync); data also optionally in iCloud. The local group container exists but Day One discourages direct access. JSON export is the right path. Media (photos/audio/video) can be optionally included in the export zip. creationDate and modifiedDate are ISO8601. Entry text is markdown. Tags and locations are first-class. Day One has ~10M users — worth prioritizing.

#### iCloud Drive — _Cloud Drive_

🟢 **High — the local path is a standard filesystem directory. Files are real files when downloaded. The main caveat is placeholder .icloud files for un-downloaded content.** · M3; M2 watch · none for user-visible folder; FDA for ~/Library paths · effort **S** · 🆕 new

- **Access:** Local mirror at ~/Library/Mobile Documents/com~apple~CloudDocs/. App-specific subfolders at ~/Library/Mobile Documents/<bundle-id>/. Files in 'stream' mode appear as .icloud placeholders when not downloaded; 'mirror' mode keeps all local.
- **Recommendation:** Build now — the user can point Trove at their iCloud Drive folder (or specific subfolders) as a watch/import source. It is just a folder. Treat undownloaded .icloud placeholders gracefully (skip or log).
- **Notes:** Files not yet downloaded appear as .~name.icloud placeholder files. Reading them triggers a download if iCloud Drive is active. If 'Optimize Mac Storage' is on, many files may be placeholders. Advise users to ensure files are downloaded before import, or only index what is local. app-specific folders (e.g., ~/Library/Mobile Documents/com~apple~Pages/) contain app data too (Pages, Numbers, Keynote) — each app's sub-schema is different.

#### Dropbox — _Cloud Drive_

🟢 **High — local folder is a standard filesystem location after the File Provider migration. Files are real files (not placeholders for locally-available ones). FDA already in Trove's grant.** · M3 (local folder read); M5 fallback via API · Full Disk Access for ~/Library/CloudStorage path · effort **S** · 🆕 new

- **Access:** Local mirror at ~/Library/CloudStorage/Dropbox/ (post-File Provider migration, macOS 12.5+). Previously ~/Dropbox; the old path is gone for migrated users. Requires Dropbox desktop app to be installed and running for sync, but files persist locally when offline. API: api.dropboxapi.com v2 (OAuth2).
- **Recommendation:** Build now — fold into the same iCloud Drive / cloud-folder watch mechanism. The path changed in 2023 (~/Library/CloudStorage/Dropbox) — handle both old and new paths.
- **Notes:** Dropbox warns against manually modifying ~/Library/CloudStorage/Dropbox but reading is safe. Online-only files show as placeholders. The Dropbox API (OAuth2, api.dropboxapi.com/2/files/list_folder) is a clean M5 fallback for users who don't have the desktop app installed. The local folder approach is simpler and requires no API credentials.

#### Google Drive for Desktop — _Cloud Drive_

🟢 **High — same local-folder mechanism as Dropbox/iCloud. API is well-maintained (docs updated May 2026).** · M3 (local folder); M5 via Drive API v3 · Full Disk Access for local folder; OAuth (Google) for API · effort **S** · 🆕 new

- **Access:** Local mirror at ~/Library/CloudStorage/Google Drive/ (macOS 12.1+, File Provider). In 'Mirror files' mode, all files are local. In 'Stream files' mode (default), only selected files are local; rest are placeholders. Also accessible via Google Drive API v3 (OAuth2): GET /drive/v3/files, files.export for Docs/Sheets/Slides.
- **Recommendation:** Build now — the local Google Drive folder folds into the same cloud-folder watch feature. The Google integration worktree already has OAuth plumbing; the Drive API v3 is a natural M5 extension.
- **Notes:** Google Docs, Sheets, Slides are not real files locally — they are .gdoc/.gsheet/.gslides stubs pointing to the web. To get content, use the Drive API files.export endpoint (e.g., export as text/plain or application/vnd.openxmlformats-officedocument.wordprocessingml.document). The local stub approach gets folder structure and metadata only. 'Mirror files' mode is required for full local copies of binary files. Drive API rate limit: 12,000 queries/minute per user for personal accounts.

#### Notion — _Notes / Workspace_

🟢 **High — both paths are live and well-documented in 2026. PATs make personal-account M5 integration practical without full OAuth app registration.** · M1 (workspace export ZIP); M5 (API with PAT) · OAuth / API key (PAT) · effort **M** · 🆕 new

- **Access:** Two paths: (1) Export: Settings > Workspace Settings > Export all workspace content → ZIP of Markdown + CSV files (can take up to 30h for large workspaces; link expires in 7 days). (2) Notion API: POST https://api.notion.com/v1/pages/:id and GET /v1/blocks/:id/children; Personal Access Tokens (PATs) available from app.notion.com/developers since May 2026. Rate limit: 2,700 requests/15 min (~3 req/s).
- **Recommendation:** Build later — M effort due to API pagination complexity and block-tree recursion. Start with the M1 workspace-export ZIP parser (Markdown is already there); add incremental API sync as a second phase.
- **Notes:** The API has no bulk-export endpoint; reconstructing a full page requires recursive block fetching. PATs (Personal Access Tokens, May 2026) simplify auth for personal use — no OAuth app registration needed. Workspace export ZIP contains Markdown + CSV; acceptable for periodic snapshots. The GET /v1/pages/:id/markdown endpoint (new 2026) can return a page as enhanced markdown but is currently restricted to public integrations. Large workspaces can take 30h to export. No offline/local DB exists.

#### Capacities — _Notes / Knowledge base_

🟢 **High — automated local export means the ZIP arrives on disk without any cloud round-trip; Trove can watch the export folder for new ZIPs. API also available.** · M1 (automated export ZIP); M5 (API) · API key / OAuth token · effort **S** · 🆕 new

- **Access:** Two paths: (1) Automated local export (released May 2025): Settings > Export — schedules daily/weekly/monthly ZIP exports directly to local disk, no cloud intermediary. Exports entire space(s). (2) API: Bearer-token OAuth 2.0 (Settings > Capacities API), REST endpoints for search, create, update, delete. Docs at docs.capacities.io.
- **Recommendation:** Build now — S effort. The automated-export-to-local-disk feature (May 2025) makes this an M2 watch-folder pattern: user configures Capacities to dump ZIPs to ~/Downloads or a watched folder; Trove ingests them.
- **Notes:** ZIP format contains structured content. API uses Bearer token obtainable in the desktop app. The offline export engine runs directly on the Mac (no server round-trip). Up to 5 automated export schedules. Free tier supports export.

#### Evernote — _Notes_

🟢 **High for M1 — ENEX is stable XML and widely parsed. M5 is Low due to OAuth 1.0 + Thrift, making it unsuitable for a polished integration.** · M1 (ENEX export); M5 possible but painful · none for M1; API key for M5 · effort **S** · 🆕 new

- **Access:** Export: File > Export Notes (Mac app) → .enex (ENML/XML) or HTML; up to 100 notes at a time or whole notebooks. Entire account exportable via notebooks. API: Evernote API uses OAuth 1.0 (not 2.0) — requires Client ID + Secret; Thrift protocol; sandboxed vs. production. Legacy API, difficult to use as library.
- **Recommendation:** Build now (M1 only) — parse .enex ZIP exports. Skip the API; OAuth 1.0 + Thrift is too painful and Evernote's API trajectory is unclear. ENEX export captures all content.
- **Notes:** ENEX is XML (ENML schema — a subset of HTML). Content is in <content> tags as CDATA. Attachments as base64 <resource> elements. Metadata: created, updated, title, tags, notebook, source-url. The evernote-backup Python library uses the API but faces the OAuth 1.0 friction. The desktop app export is the reliable path. Evernote has been declining in users but still has a large legacy base.

#### Simplenote — _Notes_

🟢 **High for M1 — simplenote.json is clean JSON with all notes, tags, creation/modification dates.** · M1 · none · effort **S** · 🆕 new

- **Access:** Export: File > Export Notes (desktop/web app) → ZIP containing individual .txt files + simplenote.json (all notes with tags and metadata). No public API as of March 2025 — Automattic acknowledged a feature request but gave no timeline.
- **Recommendation:** Build now — S effort, trivial JSON parser. No API to maintain.
- **Notes:** simplenote.json structure: array of note objects, each with content (plain text/markdown), tags (array), creationDate, lastModified, id. TXT files are the same content. Simplenote is Automattic-owned (WordPress company) — likely stable. Export covers all notes including deleted ones (marked with deleted:true).

#### Standard Notes — _Notes (encrypted)_

🟢 **High — decrypted ZIP export provides individual plain-text note files. No local DB to copy-read (always encrypted).** · M1 (decrypted ZIP export) · none for export · effort **S** · 🆕 new

- **Access:** Export: Preferences > Backups > Download Backup. Two options: (1) Encrypted JSON (one file, requires account password to decrypt, re-importable). (2) Decrypted ZIP — decrypted backup file + folder of individual plain-text notes. Local storage on device: encrypted by default (account master key in Keychain). No local plaintext DB to read directly.
- **Recommendation:** Build now — S effort. User exports decrypted ZIP; Trove parses individual note files. The encrypted export can be stored as a vault archive but offers no indexable content.
- **Notes:** Standard Notes is end-to-end encrypted; no server or local plaintext DB exists to query. Only the user-initiated decrypted export is readable. Note files in the ZIP are plain text/markdown. Extensions (Rich Text, Spreadsheets, Code) produce different formats — handle gracefully. Standard Notes is open-source (AGPL). Self-hostable sync server.

#### Google Keep — _Notes_

🟢 **High — Takeout is stable, JSON format is clean and well-documented in the community.** · M1 · none (Google account login for Takeout) · effort **S** · 📋 planned

- **Access:** Google Takeout (takeout.google.com) > select Keep > download ZIP. Contains: one HTML file per note + one JSON file per note. JSON has: title, textContent (or listContent array for checklists), color, isTrashed, isPinned, isArchived, attachments, userEditedTimestampUsec.
- **Recommendation:** Build now — S effort. Takeout JSON parser is trivial. Pairs with other Takeout imports (YouTube, Maps Timeline) — one import infrastructure.
- **Notes:** No API exists for Keep (unlike other Google Workspace products). Takeout is the only programmatic path. Timestamps are in microseconds (divide by 1000 for milliseconds). Checklist items in listContent[].text + isChecked. Images stored as separate files referenced by attachment filePath. Color codes: DEFAULT, RED, ORANGE, YELLOW, GREEN, TEAL, BLUE, CERULEAN, PURPLE, PINK, GRAY, WHITE.

#### Reflect Notes — _Notes_

🟢 **High for M1/M2 — exports are standard formats; daily local backups are automatic.** · M1 (export ZIP); M2 (daily local backup watch) · none for export · effort **S** · 🆕 new

- **Access:** Export: app menu > Export → Markdown, HTML, or JSON ZIP. API (reflect.academy/api): write-only/append-only for notes (end-to-end encrypted — server never sees plaintext, so read API is not possible). Daily automated backups to local hard drive. API uses Bearer token.
- **Recommendation:** Build now — S effort. Watch the daily backup folder (Reflect creates automatic local backups) or use the export ZIP. API is write-only so not useful for reading.
- **Notes:** End-to-end encrypted; API only allows appending. Export/backup is the correct read path. Markdown export is well-formed. Reflect targets the networked-thought / daily-notes market. Local backup location: not publicly documented but likely ~/Downloads or a user-configured path.

#### Craft — _Notes / Documents_

🟢 **High — Craft's own API (2025+) is the clean path; no reverse-engineering needed.** · M5 (API); M1 (export) · API key (Bearer token from within app) · effort **S** · 🆕 new

- **Access:** API: craft.do app Settings > API — generates a unique API endpoint per connection with configurable permissions (read specific docs, all daily notes, or full space). REST, Bearer token. Supports: search, create, update, delete documents, list collections, access daily notes. Export: Share > Export as Markdown/PDF/Word. No known local DB path (app is sandboxed Mac App Store app using CloudKit).
- **Recommendation:** Build now — S effort. Craft API is well-documented, token obtainable in-app, supports search across all documents. Pairs with Claude Code skill already written for Craft.
- **Notes:** Craft API endpoint is per-connection (not a global REST base URL) — user copies it from Settings > API. Permissions are user-controlled at connection creation. Daily notes are accessible. API supports regex search and timezone-aware date filters. CloudKit backend means no accessible local SQLite. The Craft developer docs are at docs.craft.co.

#### Ulysses — _Writing / Notes_

🟡 **Medium — iCloud path is readable with FDA; proprietary XML format needs parsing. Community tools exist (export-ulysses, ulysses-tools). Export to Markdown is the simpler M1 path.** · M3 (iCloud path read); M1 (export Markdown) · Full Disk Access for the Library path (it is under ~/Library/Mobile Documents) · effort **S** · 🆕 new

- **Access:** Local files (iCloud sync enabled): ~/Library/Mobile Documents/X5AZV975AG~com~soulmen~ulysses3/Documents/Library/. Without iCloud: ~/Library/Containers/com.soulmen.ulysses3/Data/Documents/Library/. Internal format is proprietary XML-based (.ulyz, .ulgroup). Export: File > Export → Markdown, Plain Text, Rich Text, TextBundle, HTML, DOCX, PDF, ePub. No public API.
- **Recommendation:** Build later — M1 Markdown export is the low-effort path; M3 is worthwhile only if live-sync is desired. Given Ulysses' subscription model and relatively niche user base, deprioritize vs. Bear/Drafts.
- **Notes:** Ulysses sheets are individual XML files inside the Library folder. Community Python tools exist for parsing. iCloud path requires FDA. External Folders feature (if enabled by user) writes plain .md files to a chosen location — making it effectively an Obsidian-style plain folder that Trove can watch directly. Recommend advising users to use External Folders for the cleanest integration.

#### OneNote — _Notes (Microsoft)_

🟡 **Medium — API works but requires registering an Azure AD app for OAuth2 delegated flow; page content returned as HTML (needs conversion); no bulk export endpoint. M1 export is section-by-section only (no workspace-wide dump).** · M5 (Graph API OAuth); M1 (DOCX/PDF export per section) · OAuth (Microsoft account) · effort **M** · 🆕 new

- **Access:** Microsoft Graph API: GET /me/onenote/notebooks, /sections, /pages, /pages/{id}/content (returns HTML). OAuth 2.0 delegated auth required (app-only auth removed March 31 2025). Export: File > Export → .one (proprietary binary), PDF, or DOCX per section. No local SQLite on Mac (Mac app is essentially a web wrapper).
- **Recommendation:** Build later — M effort, primarily for users in the Microsoft ecosystem. Prioritize after simpler M1 sources.
- **Notes:** App-only authentication removed March 31 2025 — delegated auth (user signs in) required. Graph API returns page content as HTML; convert to Markdown with a library. Pagination required for listing pages. The Mac app stores nothing locally useful. OneNote is popular in enterprise/education contexts — worthwhile for a general-audience app.

#### Roam Research — _Notes / Knowledge base_

🟡 **Medium — export works but is manual (no programmatic trigger from outside the browser app); the backend API is in beta with limited coverage. M1 export is reliable.** · M1 (JSON/Markdown export); M6 (MCP/CLI agent) · none for M1 · effort **S** · 🆕 new

- **Access:** Export: ... (Roam logo) > Export All > JSON or Markdown or EDN — downloads a zip with all pages. Beta Backend API (developer.ro.am): add/append blocks, retrieve pages/blocks — no full-graph export endpoint. Third-party tools: roam-research-mcp (MCP server + CLI), roam-research-private-api (browser automation).
- **Recommendation:** Build later — Roam's user base is small and declining (Obsidian has largely displaced it). M1 JSON/Markdown export parser is S effort if there is demand.
- **Notes:** JSON export is a graph structure (pages as nodes, blocks as children). Markdown export is more human-readable. No official read API beyond beta block-level endpoints. roam-research-mcp requires a Roam Graph database file and a running local server — violates the standalone constraint if used as a live sync path. The M1 export is the only constraint-compliant path.

#### Stoic (journal app) — _Journaling_

🟢 **High — JSON export is structured and well-suited for import. Re-runnable with entry deduplication by UUID.** · M1 · none · effort **S** · 🆕 new

- **Access:** Export: app menu > Import & Export → JSON (full backup with metadata, re-importable) or TXT (plain text). iCloud sync across Apple devices. No known local DB path for direct M3 access. macOS app available (2025+).
- **Recommendation:** Build later — S effort but smaller user base than Day One. Bundle with Day One JSON parser (similar structure).
- **Notes:** JSON export includes entry text, timestamps, attachments flag, mood/metrics. TXT export is text-only. iCloud syncs across iPhone, Mac, iPad, Apple Watch. Export does not include photos unless 'Include Attachments' is toggled on. Stoic is iOS-first; Mac app arrived 2025.

#### Apple Journal — _Journaling_

🟠 **Low — app is brand-new on Mac (announced June 2025, ships fall 2026); local DB format not yet documented; no export API known. Spike needed once the app ships.** · M3 (if local container found); M1 if export feature ships · Full Disk Access (speculative) · effort **M** · 🆕 new

- **Access:** App arrived on macOS Tahoe 26 (macOS 26, shipping fall 2026). Uses iCloud sync (Journal must be enabled in System Settings > Apple ID > iCloud). Local data location: not yet publicly documented; likely CloudKit container or ~/Library/Containers/com.apple.journal/. No export feature confirmed as of June 2026. No API.
- **Recommendation:** Spike first — wait for macOS Tahoe 26 to ship (fall 2026), introspect the container path and DB format, then build. iOS Journal format research (from GitHub tallmike/AppleJournaltoDayOne) may give clues.
- **Notes:** Apple Journal has been iOS/iPadOS-only since launch in December 2023. macOS Tahoe 26 brings it to Mac (announced WWDC 2025). The iOS version stores data in a CloudKit-backed container. If the Mac version follows the same pattern, there may be a local SQLite under the app container. journaling suggestions API (JournalingSuggestions framework) is iOS-only. Entry format is likely rich text / markdown with photos.

#### OneDrive — _Cloud Drive_

🟢 **High — local folder is a standard filesystem path with FDA; Graph API is well-documented.** · M3 (local folder); M5 (Graph API) · Full Disk Access for local folder; OAuth (Microsoft) for API · effort **S** · 🆕 new

- **Access:** Local mirror at ~/Library/CloudStorage/OneDrive-Personal/ (macOS, File Provider, requires OneDrive app installed). Microsoft Graph API: GET /me/drive/root/children, GET /me/drive/items/{id}/content — returns file bytes. OAuth 2.0 delegated auth.
- **Recommendation:** Build later — fold into the same cloud-folder watch feature as Dropbox/iCloud Drive. Microsoft user base is large but OneDrive for personal use is less common on Mac than iCloud/Dropbox.
- **Notes:** OneDrive uses Apple's File Provider API on macOS 12.5+. Local path: ~/Library/CloudStorage/OneDrive-Personal/ for personal accounts, ~/Library/CloudStorage/OneDrive-<org>/ for work accounts. Online-only files show as placeholders. Graph API: same OAuth flow as OneNote — bundle the Microsoft integration.

#### Box — _Cloud Drive_

🟡 **Medium — API requires registering a Box developer app; personal accounts support OAuth2 only (no CCG/JWT). Niche on Mac — Box is primarily enterprise.** · M5 (API); M3 (local folder if Box app installed) · OAuth (Box app registration); Full Disk Access for local folder · effort **M** · 🆕 new

- **Access:** Box API v2: OAuth 2.0 (personal account only supports OAuth2, not JWT/CCG). GET /files/{file_id}/content to download. GET /folders/{folder_id}/items to list. Local Box app syncs to ~/Library/CloudStorage/Box/ or ~/Box/ depending on version. No local DB.
- **Recommendation:** Icebox — Box is primarily enterprise; personal Mac users rarely use it. Revisit if there is community demand.
- **Notes:** Box Developer Console required for OAuth2 app registration. Free/personal accounts cannot use CCG or JWT, only OAuth2. Rate limits: 1,000 API calls/minute. Box Drive (desktop app) uses ~/Library/CloudStorage/Box/ on macOS 12.5+.

#### Password manager metadata (1Password, Bitwarden) — _Security / Metadata_

🟢 **High — both export formats are well-documented. The value for Trove is metadata only: vault item names, categories, URLs, notes, tags, creation/modified dates. Secrets (passwords) MUST NOT be imported.** · M1 · none (user-initiated export, requires master password to unlock) · effort **S** · 🆕 new

- **Access:** 1Password: File > Export → .1pux (JSON) or CSV. .1pux is a ZIP containing export.data (nested JSON: account > vaults > items with titles, categories, tags, URLs, notes — NO secrets in Trove). Bitwarden: Settings > Export Vault → CSV or JSON (decrypted). JSON includes name, username, URIs, notes, folders — NOT passwords in Trove output.
- **Recommendation:** Build later — S effort but requires careful UX to enforce metadata-only import (explicitly strip password/secret fields at parse time). Value is understanding what services/accounts exist, when they were created/updated.
- **Notes:** CRITICAL: Trove must strip all credential fields (password, TOTP seed, card number, SSN, etc.) at import time — only metadata enters the vault. 1PUX format documented at support.1password.com/1pux-format/. Bitwarden JSON schema is in their public docs. Keychain access is a separate source (macOS Security framework) — do not conflate. Other password managers (Dashlane, LastPass) have similar CSV exports.

#### DocuSign / e-signed documents — _Legal / Documents_

🟡 **Medium — API is comprehensive but requires DocuSign developer app registration. Most personal users have few documents; M1 manual download is sufficient.** · M5 (API); M1 (manual PDF download) · OAuth (DocuSign account) · effort **M** · 🆕 new

- **Access:** DocuSign eSignature REST API (>400 endpoints): GET /accounts/{id}/envelopes lists all envelopes; GET /envelopes/{id}/documents/{doc_id} downloads PDF. OAuth 2.0 required (DocuSign Developer account). Alternatively: manual download from DocuSign web UI (Manage > Download). Third-party bulk-export tool exists (github.com/SignRequest/docusign-exporter).
- **Recommendation:** Build later (M1 first) — add a PDF file-drop importer for signed documents with metadata extraction (parties, dates, title from filename). API integration is worthwhile only if there is demand for automated sync.
- **Notes:** DocuSign API requires developer account and OAuth app. Envelopes contain completed/voided status, signer details, dates. HelloSign is now Dropbox Sign API — similar REST interface. Adobe Sign is another common service. A generic 'signed document' PDF drop with manual metadata tagging may serve more users than a DocuSign-specific integration.

### Artifacts: Notes, Documents, Drafts & Files — cross-cutting notes

1. CLOUD-DRIVE CONVERGENCE: iCloud Drive, Dropbox, Google Drive for Desktop, and OneDrive all land at ~/Library/CloudStorage/<service>/ on macOS 12.5+ via Apple's File Provider API, all requiring FDA (already held). One generic 'cloud folder watch' feature with per-service path detection covers all four — build once, configure per service. The key gotcha is placeholder files for un-downloaded content in 'stream' mode.

2. PLAIN-FOLDER NOTE APPS: Obsidian, Logseq, and any other local-markdown vault (Roam export, Standard Notes decrypted export) are all plain folders of .md files — a single folder-import/watch code path serves all of them. Cost: S effort total for the pattern, then near-zero per new app.

3. FDA-GATED LOCAL DBs: Bear, Drafts, Apple Notes, and Ulysses all live in ~/Library/Group Containers/ or ~/Library/Mobile Documents/ — paths that require Full Disk Access. Trove already holds FDA (Safari, iMessage, Biome). No new permission prompt needed; these are free riders on the existing grant.

4. M1 EXPORT BUNDLE: Day One JSON, Evernote ENEX, Simplenote JSON, Standard Notes decrypted ZIP, Google Keep Takeout JSON, Capacities ZIP, Roam JSON/MD export, and Stoic JSON all arrive as ZIP files with structured JSON or Markdown. A common 'drop a ZIP export' intake pipeline with per-format parsers and UUID-based deduplication handles the whole category. Re-import is safe (idempotent by UUID/entry-id).

5. MICROSOFT STACK BUNDLING: OneNote (Graph API), OneDrive (local folder + Graph API), and any future Office Docs export share OAuth2 delegation against Microsoft's AAD — register once, share the token. Graph API base URL and auth are identical.

6. METADATA-ONLY RULE FOR PASSWORD MANAGERS: Any password-manager importer MUST strip all credential fields at parse time (passwords, TOTP seeds, card numbers, SSNs). Only vault structure metadata enters Trove. This is a hard requirement to document in the importer contract.

7. APPLE JOURNAL TIMING: The app ships to Mac in fall 2026 (macOS Tahoe 26). The local container format will be introspectable at that time. A deferred spike (check container path, DB format) should be queued for immediately after the OS ships. The iOS implementation (CloudKit-backed) is the likely template.

8. APPLE NOTES PROTOBUF COMPLEXITY: The gzipped-protobuf body encoding is the only non-trivial parsing challenge in this domain. The Rust prost crate can consume the reverse-engineered .proto definitions (maintained by the apple_cloud_notes_parser Ruby project and apple-notes-parser Python project, both updated for macOS 15/16). This is worth a one-time spike to validate the proto schema compiles into a prost-generated Rust type before committing to the integration.

9. TIME-SENSITIVITY: Notes/documents are generally NOT time-sensitive (unlike music scrobbling or screen time) — they can be imported at any time from exports or DBs. The one exception is if Drafts or Bear are set to encrypt on lock — capture the unencrypted DB on each sync while the user is logged in."

---

## Photos & Visual Media

Apple Photos on macOS is the richest local source: its Photos.sqlite database at ~/Pictures/Photos Library.photoslibrary/database/ holds per-asset metadata (GPS/location, timestamps, faces/people, ML quality scores, albums, scene labels), all accessible read-only via a copy-then-read pattern under Full Disk Access. A companion psi.sqlite at the same path stores on-device ML word embeddings and scene classifications. Google Photos API is substantially degraded since March 2025 — the Library API can now only read back content your own app uploaded; the new Picker API returns only user-selected items with minimal metadata (id/baseUrl/mimeType), making Takeout the only practical bulk import path. Social photo services (Flickr, Instagram, SmugMug) retain functional export/API paths. Local-ML enrichment (Apple Vision OCR/document recognition, CLIP embeddings via fastembed-rs) is fully feasible compiled into the binary. The correct stance throughout is: index metadata, geotags, and ML labels from the existing library — never duplicate the image store.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Apple Photos — library metadata (Photos.sqlite) | Local photo library | M3 | FDA — rides troved's existing grant | M | 🟢 High — well-documented schema, actively reverse-engineered (osxphotos, forensicscooter, dogsheep-photos); copy-then-read pattern already implemented for Safari/iMessage/Podcasts/Books. Table-prefix integer changes per macOS major release require a schema-probe at open time. | 📋 planned |
| Apple Photos — psi.sqlite ML scene/object labels | Local ML enrichment | M3 | FDA — same grant as Photos.sqlite | S | 🟢 High — same copy-then-read pattern. UUID int-pair conversion is documented (dogsheep-photos issue #16, theforensicscooter). Labels are Apple's own CoreML object/scene outputs (dog, beach, sunset, food, etc.) already computed for every photo. | 🆕 new |
| Apple Photos — iCloud Shared Photo Library | Local photo library | M3 | FDA — same grant as Photos.sqlite | S | 🟢 High — free rider on the Photos.sqlite collector. Zero incremental permissions. The cloudphotosd cache (~Library/Containers/com.apple.cloudphotosd/) holds shared-album thumbnails but is not needed for metadata. | 🆕 new |
| Apple Photos — Live Photos, Cinematic, and Spatial video flags | Local photo library | M3 | FDA — same grant | S | 🟢 High — these are additional columns in an already-queried table. No extra permissions, no extra copies. | 🆕 new |
| Screenshot folder — watch and OCR | Screenshots / OCR | M2 | none for ~/Desktop; FDA if user moves default to a protected path | M | 🟢 High — screenshot folder is user-accessible, Vision framework is public API. WWDC25 RecognizeDocumentsRequest adds structured extraction (tables, lists, QR codes, emails/URLs). Existing Tauri+Rust art: mirowl (Rust/Tauri + native Vision), TidyShot, ClariRec all prove the pattern. | 🆕 new |
| Camera EXIF / standalone image files | EXIF metadata | M1 | none (for user-dropped files); FDA if reading directly from camera mount | S | 🟢 High — pure-Rust EXIF parsing compiles directly into the binary. nom-exif covers HEIC (iPhone native format) and video (MOV/MP4 for Live Photos). GPS, capture timestamp, device/lens model, orientation all extractable with no external tools. | 🆕 new |
| Apple Vision framework — on-device OCR and document recognition | Local ML enrichment | M3 | none | S | 🟢 High — public API, on-device, actively enhanced. The objc2-vision crate or a Swift helper both work (EventKit bridge proved the Swift-helper pattern). Vision OCR is used in production by mirowl (Rust/Tauri), TidyShot, ClariRec, OwlOCR — all shipping macOS apps. | 🆕 new |
| Google Photos — Google Takeout export (M1 import) | Cloud photo library | M1 | none (user-initiated) | M | 🟡 Medium — export available and well-understood, but sidecar JSON naming has edge cases: for a file named IMG_1234.jpg, the sidecar may be IMG_1234.jpg.json or IMG_1234.json or IMG_1234(1).json for duplicates. One-shot import; cannot be automated (no API to trigger a new Takeout). The Library API (photoslibrary.readonly scope) was fully revoked March 31, 2025 — bulk API access is gone. | 🆕 new |
| Google Photos — Library API / Picker API | Cloud photo library | M5 | OAuth (Google account) | M | 🟠 Low — the API regression makes this infeasible for a library-level index. Picker API is a UI-flow picker, not a sync API. Returns minimal metadata (no GPS, no dates). For Trove's use case (index the full library) it is not useful. Takeout is the correct path. | 🆕 new |
| Instagram — account data export | Social photo archive | M1 | none (user-initiated) | S | 🟢 High — straightforward one-shot import; JSON is parseable. Main limitation: 48-hour turnaround, 4-day link expiry, no automation. No live API for personal photo data (Graph API personal data endpoints require app review and are restricted to approved partners). | 📋 planned |
| Flickr — API pull + data export | Photo hosting archive | M5 | OAuth (Flickr account); API key requires Pro subscription | M | 🟡 Medium — API alive and functional but Pro-only API key is a friction point (compiled-in credential only helps Pro users). Data export is available to all accounts and is the better first-version path. | 🆕 new |
| SmugMug — API pull | Photo hosting archive | M5 | OAuth (SmugMug account) | M | 🟡 Medium — API alive and functional. Niche audience (professional photographers who use SmugMug). OAuth 1.0a is old but supported. Metadata includes EXIF passthrough, captions, keywords, geotags. | 🆕 new |
| CLIP / fastembed-rs — semantic image embedding | Local ML enrichment | M3 | none | L | 🟡 Medium — technically feasible (fastembed-rs actively maintained, ONNX model downloadable at first run), but embedding a large photo library is CPU/time-intensive. More appropriate as an opt-in feature than default collection. The primary value is better served first by Apple's psi.sqlite ML labels (free, already computed) and EXIF GPS + face tags. | 🆕 new |
| 500px — export / API | Photo hosting archive | M1 | none | XL | 🔴 Blocked — no API, no bulk export. Not practically integrable without scraping (violates ToS and Trove's standalone/privacy constraints). | 🆕 new |

### Detail

#### Apple Photos — library metadata (Photos.sqlite) — _Local photo library_

🟢 **High — well-documented schema, actively reverse-engineered (osxphotos, forensicscooter, dogsheep-photos); copy-then-read pattern already implemented for Safari/iMessage/Podcasts/Books. Table-prefix integer changes per macOS major release require a schema-probe at open time.** · M3 · FDA — rides troved's existing grant · effort **M** · 📋 planned

- **Access:** ~/Pictures/Photos Library.photoslibrary/database/Photos.sqlite — copy-then-read (same WAL pattern as Safari/iMessage). Key tables: ZASSET/ZGENERICASSET (per-photo: filename, dates, GPS lat/lon, favorite, hidden, burst, live, cinematic, spatial flags); ZADDITIONALASSETATTRIBUTES (original filename, import session, dimensions, location altitude); ZCOMPUTEDASSETATTRIBUTES (ML quality/aesthetic scores — harmonics, lighting, composition, focus, symmetry — floats, drive Memories); ZPERSON + ZDETECTEDFACE + ZFACECROP (face clusters, people albums, estimated age/gender/hair); Z_26ALBUMS/Z_26ASSETS (album membership — prefix increments each macOS major version); ZSHARE + ZSHAREPARTICIPANT (iCloud Shared Photo Library).
- **Recommendation:** Build now — geotags are the single best location-history proxy Trove has, and faces/people data is unique. Metadata-only read, no image duplication.
- **Notes:** Table prefixes (Z_26ALBUMS etc.) increment each macOS major release — probe Z_PRIMARYKEY at open time to find current entity IDs, as osxphotos does. Multiple .photoslibrary bundles possible (scan ~/Pictures/). Also note the System Photo Library setting: users can have one 'system library' synced with iCloud plus additional non-synced libraries.

#### Apple Photos — psi.sqlite ML scene/object labels — _Local ML enrichment_

🟢 **High — same copy-then-read pattern. UUID int-pair conversion is documented (dogsheep-photos issue #16, theforensicscooter). Labels are Apple's own CoreML object/scene outputs (dog, beach, sunset, food, etc.) already computed for every photo.** · M3 · FDA — same grant as Photos.sqlite · effort **S** · 🆕 new

- **Access:** ~/Pictures/Photos Library.photoslibrary/database/search/psi.sqlite — companion to Photos.sqlite. Contains word_embedding table (word, extended_word columns keyed to photo UUID index). Photo UUIDs stored as two signed int64 columns (high/low 64-bit halves of the 128-bit UUID) requiring a byteswap/join to match UUID strings in Photos.sqlite. Also contains a 'collections' table with Memories and Categories metadata.
- **Recommendation:** Build now — fold into the Photos.sqlite collector as a second read pass. Apple's ML labels are a uniquely rich free signal: scene/object/activity tags for the entire library without any additional inference cost.
- **Notes:** The UUID int-pair encoding requires: uuid_string = format!('%08X-%04X-%04X-%04X-%012X', ...) after extracting the 16 bytes. psi.sqlite does not appear in osxphotos's main API — it was separately documented by forensic researchers. Schema may change across macOS versions; probe table structure at open time.

#### Apple Photos — iCloud Shared Photo Library — _Local photo library_

🟢 **High — free rider on the Photos.sqlite collector. Zero incremental permissions. The cloudphotosd cache (~Library/Containers/com.apple.cloudphotosd/) holds shared-album thumbnails but is not needed for metadata.** · M3 · FDA — same grant as Photos.sqlite · effort **S** · 🆕 new

- **Access:** Same Photos.sqlite, same ~/Pictures/Photos Library.photoslibrary/. ZSHARE and ZSHAREPARTICIPANT tables record the shared library: creation date, owner identity, up to 5 participant identities, sharing status. Assets from the shared library appear in ZASSET with a ZSHARE foreign key. Feature introduced macOS 13 / iOS 16.
- **Recommendation:** Build now — zero incremental effort once Photos.sqlite collector is built. Add ZSHARE/ZSHAREPARTICIPANT join to the output for shared library contributor attribution.
- **Notes:** Distinct from legacy iCloud Shared Albums (ZSHAREDALBUM). iCloud Shared Photo Library merges into one unified library whereas Shared Albums remain separate. Both are represented in Photos.sqlite but under different table relationships.

#### Apple Photos — Live Photos, Cinematic, and Spatial video flags — _Local photo library_

🟢 **High — these are additional columns in an already-queried table. No extra permissions, no extra copies.** · M3 · FDA — same grant · effort **S** · 🆕 new

- **Access:** Additional boolean/flag columns in ZASSET: ZISLIVEOFFBYREQUIRED / ZMEDIAMETADATATYPE for Live Photos, ZISCINDEMATICVIDEO for Cinematic mode, ZISSPATIAL for spatial video (iPhone 15 Pro+, Apple Vision Pro). All in the same Photos.sqlite, same copy-then-read pass.
- **Recommendation:** Build now — fold into the Photos.sqlite collector. Capturing counts and dates of Cinematic/Spatial/Live assets provides a useful timeline signal (e.g. 'got iPhone 15 Pro on date X — spatial video count jumps').
- **Notes:** Spatial video metadata (MV-HEVC stereo track with spatial metadata) is embedded in the MOV/MP4 file itself. The ZASSET flag is sufficient for counting; full spatial metadata extraction would require reading the video track (nom-exif or a QuickTime atom parser), which is overkill for v1.

#### Screenshot folder — watch and OCR — _Screenshots / OCR_

🟢 **High — screenshot folder is user-accessible, Vision framework is public API. WWDC25 RecognizeDocumentsRequest adds structured extraction (tables, lists, QR codes, emails/URLs). Existing Tauri+Rust art: mirowl (Rust/Tauri + native Vision), TidyShot, ClariRec all prove the pattern.** · M2 · none for ~/Desktop; FDA if user moves default to a protected path · effort **M** · 🆕 new

- **Access:** Default screenshot destination: ~/Desktop (configurable via 'defaults read com.apple.screencapture location'). Screen recordings: ~/Library/ScreenRecordings/ (macOS 10.15+). Watch both paths with inotify-equivalent (kqueue/FSEvents via the notify crate). Apple Vision VNRecognizeTextRequest (macOS 10.15+) / RecognizeDocumentsRequest (WWDC25, macOS 26) for on-device OCR — no network, no external process. Call via objc2 or a thin Swift helper. Store extracted text + filename/date, never the image bytes.
- **Recommendation:** Build later — the data-sources.md icebox entry is 'Rewind-style periodic capture' (XL, deferred). This is the narrower, privacy-safe variant: watch only user-saved screenshots, extract text only, never store image bytes. Value: code snippets, receipt totals, error messages in screenshots become searchable. Spike the Vision OCR bridge first.
- **Notes:** Do not confuse with the icebox periodic-capture (Rewind-style). This is passive watch + text extraction, not active screen capture. The Vision framework bridge will also serve the Photos-import OCR use case, so build it once and share it. RecognizeDocumentsRequest requires macOS 26 — fall back to VNRecognizeTextRequest on older versions.

#### Camera EXIF / standalone image files — _EXIF metadata_

🟢 **High — pure-Rust EXIF parsing compiles directly into the binary. nom-exif covers HEIC (iPhone native format) and video (MOV/MP4 for Live Photos). GPS, capture timestamp, device/lens model, orientation all extractable with no external tools.** · M1 · none (for user-dropped files); FDA if reading directly from camera mount · effort **S** · 🆕 new

- **Access:** Any JPEG/HEIC/TIFF/PNG/MOV/MP4 dropped into the vault inbox or imported from a camera SD card. Pure Rust libraries on crates.io: nom-exif (JPEG/HEIF/HEIC/TIFF/MOV/MP4/WebM/MKV — GPS, timestamps, camera model, lens); kamadak-exif (TIFF/JPEG/HEIF/PNG/WebP); little_exif (read+write: JPEG/PNG/HEIC/JXL/TIFF/WebP); libheif-rs (safe wrapper for HEIC/HEIF decode + metadata). All actively maintained 2024–2025.
- **Recommendation:** Build now as part of the Photos.sqlite collector — also offer a standalone 'drop image files here' path that reads EXIF directly for users who manage photos outside Apple Photos. Minimal incremental effort given available Rust libraries.
- **Notes:** HEIC is the default format for iPhone photos. nom-exif handles HEIC GPS extraction including byteswap of iPhone's big-endian JPEG variant. For video files (MOV/MP4), nom-exif reads QuickTime/MP4 atoms for metadata. exiftool-rs (pure Rust reimplementation of ExifTool, 93 format readers including CR2/RAW) is the heavy-duty option for professional camera RAW formats.

#### Apple Vision framework — on-device OCR and document recognition — _Local ML enrichment_

🟢 **High — public API, on-device, actively enhanced. The objc2-vision crate or a Swift helper both work (EventKit bridge proved the Swift-helper pattern). Vision OCR is used in production by mirowl (Rust/Tauri), TidyShot, ClariRec, OwlOCR — all shipping macOS apps.** · M3 · none · effort **S** · 🆕 new

- **Access:** macOS public framework (no entitlement needed). VNRecognizeTextRequest: extract text + bounding boxes from any image, 26 languages, accurate/fast modes. RecognizeDocumentsRequest (new WWDC25, macOS 26+): structured document recognition — tables, lists, paragraphs, QR codes, phone/email/URL extraction. Call from Rust via objc2 bindings (objc2-vision crate) or a thin Swift helper compiled into the bundle. Runs fully on-device.
- **Recommendation:** Spike first — determine bridge cost (objc2-vision vs. Swift helper). Once bridged, the Vision OCR capability serves both screenshot indexing and imported-image OCR. This is a shared infrastructure piece. RecognizeDocumentsRequest (macOS 26+) is a forward-looking upgrade for structured extraction.
- **Notes:** VNRecognizeTextRequest has been stable since macOS 10.15. RecognizeDocumentsRequest is macOS 26+ only — guard behind version check and fall back to VNRecognizeTextRequest. The same bridge serves both use cases. Consider batching OCR in a background task to avoid blocking the owner loop.

#### Google Photos — Google Takeout export (M1 import) — _Cloud photo library_

🟡 **Medium — export available and well-understood, but sidecar JSON naming has edge cases: for a file named IMG_1234.jpg, the sidecar may be IMG_1234.jpg.json or IMG_1234.json or IMG_1234(1).json for duplicates. One-shot import; cannot be automated (no API to trigger a new Takeout). The Library API (photoslibrary.readonly scope) was fully revoked March 31, 2025 — bulk API access is gone.** · M1 · none (user-initiated) · effort **M** · 🆕 new

- **Access:** takeout.google.com → select Google Photos → download ZIP(s). Format: original image/video files + per-item JSON sidecar (IMG_1234.jpg.json or IMG_1234.json) with: title, description, creationTime, photoTakenTime (epoch seconds), geoData {latitude, longitude, altitude, latitudeSpan, longitudeSpan}, people (face tags), url, googlePhotosOrigin. EXIF in image files is often stripped or wrong; JSON sidecar is authoritative for dates and GPS.
- **Recommendation:** Build later — valuable for users who use Google Photos as their primary library. M1 import pattern already exists. Parse JSON sidecars (not EXIF) for dates/GPS. Warn users about the sidecar naming edge cases and the multi-part ZIP structure.
- **Notes:** Google Photos Takeout often splits into multiple ZIPs if the library is large. Album membership is in separate album-metadata JSON files, not in per-photo sidecars. The naming inconsistency (google-photos-exif, metadatafixer.com document this) is the main implementation gotcha. ExifTool's -geotag option can merge sidecars back into EXIF, but Trove should read the JSON directly.

#### Google Photos — Library API / Picker API — _Cloud photo library_

🟠 **Low — the API regression makes this infeasible for a library-level index. Picker API is a UI-flow picker, not a sync API. Returns minimal metadata (no GPS, no dates). For Trove's use case (index the full library) it is not useful. Takeout is the correct path.** · M5 · OAuth (Google account) · effort **M** · 🆕 new

- **Access:** As of March 31, 2025: photoslibrary.readonly, photoslibrary, and photoslibrary.sharing scopes are revoked (403 PERMISSION_DENIED). The Library API can now only read content your own app uploaded. The new Picker API (photospicker.mediaitems.readonly scope) allows user-selected items only: returns id, baseUrl (60-min expiry), mimeType per picked item — no GPS, no dates, no album structure.
- **Recommendation:** Skip — API regression is permanent. Direct users to Takeout for Google Photos bulk import. Note in the UI that the Google Photos API no longer allows library-wide reads and link to the Takeout export flow.
- **Notes:** This is a significant regression. Third-party apps (gphotos-sync, etc.) all broke in March 2025. There is no migration path for library-wide read access — Google explicitly restricted it to the Picker (user-selects-each-time model). Do not invest in an M5 Google Photos connector.

#### Instagram — account data export — _Social photo archive_

🟢 **High — straightforward one-shot import; JSON is parseable. Main limitation: 48-hour turnaround, 4-day link expiry, no automation. No live API for personal photo data (Graph API personal data endpoints require app review and are restricted to approved partners).** · M1 · none (user-initiated) · effort **S** · 📋 planned

- **Access:** Profile → Settings & privacy → Accounts Center → Your information and permissions → Export Your Information → Export to device → JSON format. Provides: posts (photos + captions + timestamps + location tags), stories archive, reels, liked posts, followers/following, DMs. Download link emailed within 48 hours, expires after 4 days. JSON format is well-structured; HTML is for human reading only.
- **Recommendation:** Build later — low effort M1 import; already in data-sources.md as 'build when the user wants the history in'. The JSON structure includes photo timestamps and any location tags the user added, plus captions (text content for search).
- **Notes:** Instagram's export JSON naming: posts are in 'content/posts_1.json' etc. Media files are included in the ZIP. The export does not include original full-resolution files if Instagram re-compressed them. Captions and comments are the primary text-searchable content.

#### Flickr — API pull + data export — _Photo hosting archive_

🟡 **Medium — API alive and functional but Pro-only API key is a friction point (compiled-in credential only helps Pro users). Data export is available to all accounts and is the better first-version path.** · M5 · OAuth (Flickr account); API key requires Pro subscription · effort **M** · 🆕 new

- **Access:** API: api.flickr.com/services/rest/?method=flickr.photos.search&user_id=me — photo metadata (id, title, tags, dates, GPS if user-enabled, views, faves). Auth: OAuth 1.0a. API key requires Pro subscription. Data export: Settings → Request my Flickr Data → ZIP download (original photo files + JSON sidecar metadata, same EXIF-passthrough pattern as Takeout). Export processing: hours to weeks. Rust: no official crate; flickcurl is a C library; OAuth 1.0a can be implemented with the oauth1 crate.
- **Recommendation:** Build later — niche audience (Flickr Pro users with significant libraries). Prefer the M1 export path as the first version. API pull is the upgrade for continuous sync.
- **Notes:** Flickr Pro API key requirement means the M5 path requires users to supply their own API key in the settings UI. Export ZIP includes JSON sidecars with all metadata including GPS (when present), tags, album membership. Free accounts cannot download images larger than 1024px via the API bulk download, but the export includes originals.

#### SmugMug — API pull — _Photo hosting archive_

🟡 **Medium — API alive and functional. Niche audience (professional photographers who use SmugMug). OAuth 1.0a is old but supported. Metadata includes EXIF passthrough, captions, keywords, geotags.** · M5 · OAuth (SmugMug account) · effort **M** · 🆕 new

- **Access:** api.smugmug.com/api/v2/ REST API. Endpoints: /api/v2/user/<nickname>!albums (list albums), /api/v2/album/<id>!images (list images with metadata), /api/v2/image/<id>!sizedetails (download URLs by size). Auth: OAuth 1.0a. API key available to all SmugMug subscribers. Original-size download requires OAuth access token if album is not public. Rust: crates.io/crates/smugmug (low-maintenance wrapper).
- **Recommendation:** Icebox — very niche. Build only if there is clear user demand. The pattern is identical to Flickr; if Flickr is built first, SmugMug is a low-effort addition.
- **Notes:** SmugMug API requires users to supply their own API key (no keyless access). Unlike Flickr, there is no bulk export feature — the API is the only path. Video downloads are supported at the same URL structure.

#### CLIP / fastembed-rs — semantic image embedding — _Local ML enrichment_

🟡 **Medium — technically feasible (fastembed-rs actively maintained, ONNX model downloadable at first run), but embedding a large photo library is CPU/time-intensive. More appropriate as an opt-in feature than default collection. The primary value is better served first by Apple's psi.sqlite ML labels (free, already computed) and EXIF GPS + face tags.** · M3 · none · effort **L** · 🆕 new

- **Access:** crates.io/crates/fastembed — pure Rust, uses pykeio/ort (ONNX Runtime) for inference. Supports ImageEmbeddingModel::ClipVitB32 and other CLIP variants. Runs fully on-device CPU inference (Metal/CoreML acceleration possible via ort features). Embeds images into a 512-d vector space shared with text — enables natural-language image search ('find photos with mountains') without any cloud call. Model download ~300 MB on first run.
- **Recommendation:** Icebox — build psi.sqlite label extraction and EXIF/GPS first (free, already computed by Apple). CLIP embeddings are the advanced tier for semantic search beyond what Apple's ML has already labeled. Revisit when the local-LLM / embedding pipeline is in place for v0.2.
- **Notes:** fastembed-rs v4+ supports both text and image embeddings in the same embedding space, which is the key CLIP property. A ~300 MB ONNX model must be downloaded and cached. Embedding a 50k-photo library would take significant time on CPU — needs batching and a background task. The visual-search crate (crates.io) is an alternative but less maintained.

#### 500px — export / API — _Photo hosting archive_

🔴 **Blocked — no API, no bulk export. Not practically integrable without scraping (violates ToS and Trove's standalone/privacy constraints).** · M1 · none · effort **XL** · 🆕 new

- **Access:** 500px shut down its public API in 2018. No official bulk data export mechanism exists. The service was acquired by Visual China Group. Individual photo downloads possible via the website but no programmatic bulk access.
- **Recommendation:** Skip — no API, no export path. Not feasible.
- **Notes:** 500px's API was closed to new registrations in 2018 and fully shut down. The platform has declined significantly in Western markets since the Visual China Group acquisition. Users who want their 500px photos should be directed to use browser developer tools or third-party migration tools to retrieve their own content manually.

### Photos & Visual Media — cross-cutting notes

1. FDA is the single gate: Photos.sqlite, psi.sqlite, and screen recordings all live under paths already accessible once troved has Full Disk Access (already granted for iMessage/Safari/Screen Time). No new permission prompt is needed — just add the read paths to the existing grant. 2. The table-prefix problem: Photos.sqlite uses version-stamped join-table names (Z_26ALBUMS etc.) that increment each macOS major release. Probe Z_PRIMARYKEY at open time for entity IDs, exactly as osxphotos does — build this once as a shared helper in trove-core. 3. Copy-then-read pattern already implemented for Safari, iMessage, Podcasts, and Books; Photos.sqlite + psi.sqlite slot straight in with the same copy-WAL-then-open approach. 4. No image duplication: the correct stance is metadata-only — GPS coordinates, timestamps, ML labels, OCR text, and face-cluster IDs go into JSONL; image paths reference the live library. Never copy image bytes into the vault. 5. Google Photos API regression (March 2025) is permanent: the Library API can no longer read user photos; the Picker API returns only user-selected items with no GPS or dates. Takeout (M1) is now the only viable bulk path. Do not plan an M5 Google Photos connector. 6. Apple Vision OCR (on-device, public API, no entitlement) is the correct local-ML path for screenshots and imported images; bridge it once and share it across both screenshot-watch and EXIF-import collectors. fastembed-rs/CLIP is a v0.2 enrichment layer. 7. Pure Rust EXIF libraries (nom-exif covers JPEG/HEIC/MOV/MP4/WebM; kamadak-exif for TIFF/PNG; little_exif for read+write) compile directly into the binary — no system ExifTool dependency needed. 8. Social photo exports (Instagram, Flickr, Google Takeout) all follow the M1 import pattern already established for bank CSVs and health exports. A generic 'drop ZIP export here' import-registry can host photo-archive parsers as plugins alongside the existing importers.

---

## Media: Music, Podcasts, Video & TV

This domain is rich with buildable integrations spanning all three major categories: music listening history, podcast playback history, and video/TV watch history. The strongest sources have either free-keyless read APIs (Last.fm, ListenBrainz, Trakt) or straightforward GDPR/account exports (Netflix, Spotify extended history, IMDb, Letterboxd, YouTube Takeout). Several are already built or planned. The biggest structural hurdles are Spotify's February 2026 API lockdown (still usable for personal app flows but requires Premium and a registered app), the macOS 15.4 MediaRemote entitlement restriction for universal now-playing (a Perl-adapter workaround exists and is already in a Rust crate), and the fragmented streaming video landscape where Disney+/HBO/Prime/Hulu offer no open APIs and only slow/manual data-request exports. Last.fm and Trakt stand out as universal aggregators worth prioritizing — they collect cross-service history by design and have clean, well-documented APIs with no key requirement (Last.fm) or PKCE OAuth (Trakt).

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Last.fm | Music Aggregator | M5 | API key (free, self-registered); no OAuth needed for read-only history of public/any user | S | 🟢 High — free, keyless read for any public profile; paginated full history back to account creation; stable API unchanged in years | 📋 planned |
| Trakt.tv | TV/Film Aggregator | M5 | OAuth (PKCE; free client_id); public history endpoints work without auth for public profiles | S | 🟢 High — well-documented, stable API; free tier supports full history (100k entries cap noted in 2026 limits forum); PKCE flow is app-distribution-safe with compiled-in credentials | 🆕 new |
| Spotify | Music Streaming | M1; M5 fallback for live 50-item window | Account export: account login only. API: OAuth 2.0; Feb 2026 change requires Premium account for dev-mode apps; 5-user dev-mode limit; extended access (>5 users) requires registered business + 250k MAU | M | 🟢 High for export path; Medium for live API — Feb 2026 policy change restricts dev-mode to Premium users and limits test users to 5; for a personal-use Trove app running locally this is workable (user authenticates their own account, it's 1 user) | 📋 planned |
| ListenBrainz | Music Aggregator | M5 | None for reading public profiles; user token (from listenbrainz.org account) for writes | S | 🟢 High — fully open-source MetaBrainz project, no commercial restrictions, clean JSON API, paginated full history; rate-limited via headers | 🆕 new |
| Netflix | Video Streaming | M1 | Account login; no special permissions | S | 🟢 High — official CSV export is instant and comprehensive; no API needed | 🆕 new |
| Letterboxd | Film Tracking | M1; M2 possible via RSS polling | Account login for CSV export; no auth for public RSS | S | 🟢 High for CSV/RSS; Blocked for API (explicitly rejects personal/data-analysis projects as of 2026) | 🆕 new |
| IMDb | Film/TV Ratings | M1 | Account login; no special permissions | S | 🟢 High — official, instant CSV export; reliable and stable | 🆕 new |
| Overcast | Podcast Player | M5 | Overcast account credentials (email/password login to overcast.fm); no OAuth — session cookie | M | 🟢 High — well-documented unofficial endpoint that has been stable for years; per-episode played timestamps available | 🆕 new |
| Pocket Casts | Podcast Player | M5 | Pocket Casts account credentials; unofficial API, no public auth docs | M | 🟡 Medium — unofficial API works as of 2025 community posts; returns up to 100 recent items with no timestamp of when played; history depth and reliability not guaranteed | 🆕 new |
| Apple Podcasts | Podcast Player | M3 | Full Disk Access (FDA) — this path is in a Group Container, accessible with FDA | M | 🟢 High — local SQLite, well-known path, FDA already needed for other Trove collectors (iMessage, etc.); schema has remained stable across macOS versions | ✅ built |
| Shazam | Music Discovery | M3; M1 fallback via privacy export | FDA for local DB read; no additional permissions. Privacy export requires Shazam account login | S | 🟢 High — local SQLite is accessible with FDA; schema is simple and stable; iCloud sync means iPhone Shazams appear on Mac within minutes | 🆕 new |
| Plex Media Server | Local Media Server | M3; M5 via local API | No system TCC; Plex app must be installed and running (but Trove reads the SQLite directly without requiring Plex running = M3). Full history requires Plex Pass subscription for the dashboard view; raw DB data does not. | M | 🟡 Medium — Plex must be installed by the user; not universally present; but for users who have it the local SQLite approach is robust and does not require Plex to be running | 🆕 new |
| Jellyfin | Local Media Server | M5; M3 fallback | Local API key (from Jellyfin admin dashboard); no system TCC | M | 🟡 Medium — Jellyfin is less common than Plex on macOS but growing; free and open-source; local API is well-documented (Swagger at localhost:8096/api-docs/swagger/index.html) | 🆕 new |
| YouTube (watch history) | Video Platform | M1 | Google account login for Takeout; no special permissions | S | 🟢 High — official export, reliable, comprehensive history; same Takeout path as YouTube Music | 📋 planned |
| YouTube Music | Music Streaming | M1 | Google account login for Takeout | S | 🟢 High — official export path via Google Takeout; straightforward JSON | 🆕 new |
| Tidal | Music Streaming | M5; M1 fallback | OAuth 2.1 PKCE; developer account at developer.tidal.com | M | 🟡 Medium — favorites/playlists accessible via official API; listening HISTORY is not available via any official endpoint as of 2026. GDPR export is the only path to historical play data. | 🆕 new |
| Deezer | Music Streaming | M5 | OAuth; developer app at developers.deezer.com | M | 🟡 Medium — official API exists with user history endpoint; Deezer is popular in Europe and less common in North America; timestamp recovery for full history is reportedly still in development (Deezer support page) | 🆕 new |
| Snipd | Podcast Player | M2; M1 fallback | None for Obsidian/file-based export; requires Snipd premium for full transcript export | M | 🟡 Medium — no API; export to Obsidian vault (a watched folder) is the most automatable path; requires user to configure Snipd to sync to a vault folder that Trove also watches | 🆕 new |
| Amazon Prime Video | Video Streaming | M1 | Amazon account login; no special permissions | S | 🟡 Medium — data request technically available but buried in Amazon's privacy portal; format is CSV with good fields including duration. No public API for viewing history. | 🆕 new |
| Simkl | TV/Film/Anime Aggregator | M5 | OAuth 2.0 PKCE; free developer app registration at simkl.com | S | 🟢 High — clean API, PKCE flow suitable for desktop apps, free tier, comprehensive watch history including anime | 🆕 new |
| MediaRemote (Universal Now-Playing) | OS-Level Now Playing | M4 | No TCC required; but macOS 15.4+ requires the Perl-adapter workaround or JXA scripting bridge — cannot call MediaRemote directly from unsigned/non-entitled binary | M | 🟡 Medium — macOS 15.4 entitlement change is a real blocker for the direct approach; Perl-adapter workaround in mediaremote-rs crate is functional but architecturally fragile (depends on system Perl having entitlement; could change in future macOS) | 📋 planned |
| Disney+ / Hulu / HBO Max (Max) | Video Streaming | M1 | Account login at privacy portals; no special permissions | M | 🟠 Low — no official documented export format; privacy portal requests are slow (up to 30 days), format is undocumented and variable, link expiration is short; Disney+/Hulu profile integration announced in 2026 may shift export options | 🆕 new |
| TV Time | TV Tracking | M1 | TV Time account login for browser extension; GDPR request via app | M | 🟡 Medium — Chrome extension approach works but is fragile (internal API can change); GDPR request is unreliable in format; TV Time has been losing users to Trakt and Simkl | 🆕 new |
| SoundCloud | Music Streaming | M5 | OAuth 2.0; developer app registration required (approval not guaranteed) | L | 🟠 Low — SoundCloud activities API is social (likes/reposts) not passive playback history; no 'recently listened' endpoint; developer app approvals are slow or blocked for new registrants; service has had financial instability | 🆕 new |
| Pandora | Music Radio | M1 | Account login for privacy request | L | 🔴 Blocked — developer API closed to new applicants; no official export; Pandora's market share has declined sharply (radio-style, not on-demand); privacy request path is undocumented | 🆕 new |
| Bandcamp | Music Purchase | M1 | Account login for Chrome extension scrape | M | 🟡 Medium for purchase history (Chrome extension works); Low for listening history (does not exist as a concept in Bandcamp — you own files and play them locally) | 🆕 new |
| Navidrome / Subsonic (self-hosted music) | Local Music Server | M3; M5 via local API | Local API credentials (username/password); no system TCC for API; FDA for direct SQLite access | M | 🟡 Medium — niche audience (self-hosted music server users); Navidrome 0.59+ has native history; Subsonic API is stable and well-documented; very similar architecture to Plex/Jellyfin | 🆕 new |

### Detail

#### Last.fm — _Music Aggregator_

🟢 **High — free, keyless read for any public profile; paginated full history back to account creation; stable API unchanged in years** · M5 · API key (free, self-registered); no OAuth needed for read-only history of public/any user · effort **S** · 📋 planned

- **Access:** REST API — GET https://ws.audioscrobbler.com/2.0/?method=user.getRecentTracks&user=USERNAME&api_key=KEY&limit=200&page=N&from=UNIX_TS; paginate with from/page to fetch full history; free API key from last.fm/api/account/create; no user-level OAuth required for read
- **Recommendation:** Build now — highest-value music source in this domain; acts as a universal aggregator for any player that scrobbles (Spotify, Apple Music, Tidal, etc.). Pairs naturally with the existing Apple Music scrobbler. One-time backfill + incremental poll with from= watermark.
- **Notes:** Rate limit: 5 req/s sustained over 5-min window. Max 200 tracks/call. Full history for heavy listeners (>100k scrobbles) needs many pages but is achievable with watermark polling. Also supports user.getTopTracks, user.getWeeklyTrackChart for rich aggregates. Returns album, artist, track, timestamp per play. No auth required for public profiles; sk token required only for write (scrobbling) or private profile reads. Users who scrobble from any source (Spotify plugin, Last.fm app, Navidrome, etc.) get a unified stream automatically.

#### Trakt.tv — _TV/Film Aggregator_

🟢 **High — well-documented, stable API; free tier supports full history (100k entries cap noted in 2026 limits forum); PKCE flow is app-distribution-safe with compiled-in credentials** · M5 · OAuth (PKCE; free client_id); public history endpoints work without auth for public profiles · effort **S** · 🆕 new

- **Access:** REST API v2 — https://api.trakt.tv/sync/history?type=movies|shows&start_at=ISO&end_at=ISO (OAuth) or GET /users/{username}/history/{movies|shows} (public username, no auth). OAuth 2.0 PKCE flow; client_id from https://trakt.tv/oauth/applications/new (free). Response: JSON with watched_at timestamp, movie/show title, TMDB/IMDB IDs
- **Recommendation:** Build now — the Last.fm equivalent for TV and movies. Many users already use Trakt via Plex/Infuse/Emby integrations. Full history, ratings, watchlists, collection all available. Pairs with Netflix/Prime export imports to backfill pre-Trakt history.
- **Notes:** 2026 Trakt forum post confirms watched history cap at 100,000 items for both free and VIP — still plenty for most users. VIP ($3/mo) is needed for watchlist >250 items but not for history reads. GET /users/{slug}/history is the core endpoint. Also provides ratings, watchlist, collection endpoints. Trakt IDs map to IMDB/TMDB/TVDB for cross-referencing. Trakt is also a universal scrobbler — users can configure Plex, Kodi, Infuse, and others to push plays to Trakt, making it a natural aggregation hub like Last.fm.

#### Spotify — _Music Streaming_

🟢 **High for export path; Medium for live API — Feb 2026 policy change restricts dev-mode to Premium users and limits test users to 5; for a personal-use Trove app running locally this is workable (user authenticates their own account, it's 1 user)** · M1; M5 fallback for live 50-item window · Account export: account login only. API: OAuth 2.0; Feb 2026 change requires Premium account for dev-mode apps; 5-user dev-mode limit; extended access (>5 users) requires registered business + 250k MAU · effort **M** · 📋 planned

- **Access:** GDPR export: Spotify Account > Privacy settings > Download your data > Extended Streaming History (JSON); delivers StreamingHistory_*.json with ms_played, track_name, artist_name, album_name, ts per play. Live API (personal app): GET /me/player/recently-played (scope user-read-recently-played) — 50-item rolling window, cursor-paginated
- **Recommendation:** Build now — M1 import for full history (GDPR JSON); M5 live polling for ongoing capture. The 50-item recently-played API is still active and sufficient for incremental sync if polled frequently enough. The Feb 2026 API lockdown removed catalog endpoints but did NOT remove user listening endpoints. Note: also integrate via Last.fm if user scrobbles Spotify.
- **Notes:** GDPR export takes 1–5 days; contains complete lifetime history as multi-file JSON. recently-played API caps at ~1275 items via cursor pagination — not sufficient for backfill alone; use GDPR export for that. Extended Streaming History includes ms_played (good for 'skipped' detection at <30s). Feb 2026 removal of 15 endpoints did not affect recently-played, saved tracks, or user profile. API also provides currently-playing (scope user-read-currently-playing) for live now-playing integration.

#### ListenBrainz — _Music Aggregator_

🟢 **High — fully open-source MetaBrainz project, no commercial restrictions, clean JSON API, paginated full history; rate-limited via headers** · M5 · None for reading public profiles; user token (from listenbrainz.org account) for writes · effort **S** · 🆕 new

- **Access:** REST API — GET https://api.listenbrainz.org/1/user/{username}/listens?min_ts=UNIX&max_ts=UNIX&count=100; no auth required for reads of public profiles; user token needed for submitting listens
- **Recommendation:** Build now (alongside Last.fm) — growing open alternative to Last.fm with MusicBrainz ID enrichment on every listen. Many audiophiles prefer it. Same poll pattern as Last.fm with watermark. Small incremental effort given Last.fm collector would share the same pattern.
- **Notes:** Returns listen timestamps, track name, artist name, release name, plus MusicBrainz recording/artist/release GUIDs when matched. Well-documented OpenAPI spec. Free, open-source, no rate limit caps published (just headers). Navidrome, Beets, and many scrobblers now support ListenBrainz natively, making it a natural second aggregator alongside Last.fm.

#### Netflix — _Video Streaming_

🟢 **High — official CSV export is instant and comprehensive; no API needed** · M1 · Account login; no special permissions · effort **S** · 🆕 new

- **Access:** Account export: netflix.com/account > Privacy > Viewing activity > 'Download all' — instant CSV download (title, date watched, per-profile). For complete history: netflix.com > Privacy Settings > 'Request information about your account' > select 'Viewing activity' — ZIP containing fuller data, ready in ~7–14 minutes
- **Recommendation:** Build now — dead simple M1 import. CSV has title + date; no duration or episode data in the quick export. The deeper account data request provides fuller data. Should be the first streaming video import built.
- **Notes:** CSV columns: Title, Date. Episode names are included in the title string (e.g. 'Stranger Things: Season 1: Chapter 1'). No playback duration in quick export. Each profile must be exported separately. No public API. The 'Request information' path yields richer JSON with per-device, duration data but requires waiting and a link that expires in 72h. Recommend importing the quick CSV as M1 and optionally the richer export.

#### Letterboxd — _Film Tracking_

🟢 **High for CSV/RSS; Blocked for API (explicitly rejects personal/data-analysis projects as of 2026)** · M1; M2 possible via RSS polling · Account login for CSV export; no auth for public RSS · effort **S** · 🆕 new

- **Access:** Account export: letterboxd.com/USERNAME/settings/data or letterboxd.com/user/exportdata — ZIP with diary.csv (date, rating, film title, letterboxd_uri, rewatch), watched.csv, ratings.csv, reviews.csv, lists.csv. Also: RSS feed at letterboxd.com/USERNAME/rss/ (last 50 diary entries, public profiles only). Official API: api-docs.letterboxd.com — request-only, personal/data-analysis projects explicitly rejected
- **Recommendation:** Build now (M1 CSV import) — the export is excellent quality with diary dates, ratings, and full film metadata. Skip the official API application — it won't be approved. RSS polling is a viable M2 for ongoing capture of new diary entries (50-entry limit is fine for recent activity).
- **Notes:** Diary.csv is the richest file: watched_date, rating (0.5–5 in 0.5 steps), film_title, year, letterboxd_uri, rewatch flag. The official API is restricted but the export covers everything Trove needs. RSS feed URL: https://letterboxd.com/{username}/rss/ — works for public profiles with no auth. For private profiles only M1 export works. Letterboxd is the dominant film diary app; this is high-value for any film enthusiast.

#### IMDb — _Film/TV Ratings_

🟢 **High — official, instant CSV export; reliable and stable** · M1 · Account login; no special permissions · effort **S** · 🆕 new

- **Access:** Account export: imdb.com/user/urXXXXXXXX/ratings (or Your Ratings page) > three-dot menu > Export — CSV download instantly. Also available for Watchlist and custom Lists. Download link generated at imdb.com/exports/
- **Recommendation:** Build now — quick win; IMDb is the default rating tool for most users; CSV contains Const (IMDB ID), Title, Year, Rating, Date Rated, Title Type. Low engineering effort.
- **Notes:** CSV columns: Const, Your Rating, Date Rated, Title, URL, Title Type, IMDb Rating, Runtime (mins), Year, Genres, Num Votes, Release Date, Directors. Export is per-list (ratings, watchlist, each custom list separately). No public API for user data. IMDb IDs can be cross-referenced with Trakt/Letterboxd data.

#### Overcast — _Podcast Player_

🟢 **High — well-documented unofficial endpoint that has been stable for years; per-episode played timestamps available** · M5 · Overcast account credentials (email/password login to overcast.fm); no OAuth — session cookie · effort **M** · 🆕 new

- **Access:** Authenticated OPML export: https://overcast.fm/account/export_opml/extended (requires overcast.fm session cookie or login); returns XML with all subscribed podcasts + per-episode played status and timestamps. 'All data' OPML includes playlist membership. Rate limit: ~10 requests/day per Marco Arment
- **Recommendation:** Build now — Overcast is the leading iOS podcast app for power users; the extended OPML is the best podcast listening history available from any app. Auth is via session cookie (username/password POST to overcast.fm/login); 10/day rate limit is fine for a periodic sync.
- **Notes:** The OPML XML uses custom Overcast namespace attributes: overcast:progress (seconds), overcast:played (0/1), overcast:addedDate. Episode GUIDs and enclosure URLs are included for deduplication. A GitHub project (overcast-to-sqlite) already parses this into SQLite — useful reference. Does not include playback duration/speed or total listen time directly. Standard OPML (subscriptions only) at https://overcast.fm/account/export_opml/subscriptions is public and keyless.

#### Pocket Casts — _Podcast Player_

🟡 **Medium — unofficial API works as of 2025 community posts; returns up to 100 recent items with no timestamp of when played; history depth and reliability not guaranteed** · M5 · Pocket Casts account credentials; unofficial API, no public auth docs · effort **M** · 🆕 new

- **Access:** Unofficial API: https://api.pocketcasts.com/user/history (POST with email/password JSON body returns recent episodes, up to 100 items); no official public API or export. App database export requires contacting support. No official GDPR export for listening history
- **Recommendation:** Build later — unofficial API is fragile and only 100 items with no play timestamps. Pocket Casts is owned by Automattic (WordPress); they have an open GitHub issue for export (#654). Check for official export before building. Overcast is a better-return effort for podcast data.
- **Notes:** The unofficial API at api.pocketcasts.com is what the web app uses; can be reverse-engineered. History endpoint returns episode title, podcast title, played status but reportedly lacks precise played timestamps. Pocket Casts has an Android SQLite database that CAN be exported via backup, but iOS users have no equivalent. Feature request for full data export has been open since 2020.

#### Apple Podcasts — _Podcast Player_

🟢 **High — local SQLite, well-known path, FDA already needed for other Trove collectors (iMessage, etc.); schema has remained stable across macOS versions** · M3 · Full Disk Access (FDA) — this path is in a Group Container, accessible with FDA · effort **M** · ✅ built

- **Access:** Local SQLite DB at ~/Library/Group Containers/243LU875E5.groups.com.apple.podcasts/Documents/MTLibrary.sqlite; key table: ZMTEPISODE with ZLASTDATEPLAYED, ZPLAYCOUNT, ZDURATION, ZTITLE, plus ZMTPODCAST join; read-only copy before querying
- **Recommendation:** Already built per domain brief. Gaps to consider: add ZPLAYCOUNT and ZLASTDATEPLAYED alongside duration for richer episode-level stats.
- **Notes:** Schema: ZMTEPISODE.ZLASTDATEPLAYED is a Core Data timestamp (seconds since Jan 1, 2001). ZPLAYCOUNT tracks total plays. ZDURATION in seconds. ZMTPODCAST.ZTITLE for feed name. Note: Apple Podcasts does not record a full per-play history — only the most recent play date and a count. For episode-level timestamped history, Overcast is superior. Podcast audio files stored at ~/Library/Group Containers/243LU875E5.groups.com.apple.podcasts/Library/Cache/.

#### Shazam — _Music Discovery_

🟢 **High — local SQLite is accessible with FDA; schema is simple and stable; iCloud sync means iPhone Shazams appear on Mac within minutes** · M3; M1 fallback via privacy export · FDA for local DB read; no additional permissions. Privacy export requires Shazam account login · effort **S** · 🆕 new

- **Access:** Local SQLite DB at ~/Library/Containers/com.shazam.mac.Shazam/Data/Documents/ShazamDataModel.sqlite (macOS app). Table: ZSHTAGRESULTMO contains all Shazams with title, artist, timestamp. iCloud-synced: songs identified via Control Center Music Recognition also appear here. Privacy export: shazam.com/privacy > 'Download Your Data' — email delivery of JSON/CSV (SyncedShazams.csv)
- **Recommendation:** Build now — low effort, high signal for music discovery history. The local DB approach (M3) gives real-time access without any auth. Shazams auto-sync from iPhone via iCloud. The privacy export (M1) is a good initial backfill path.
- **Notes:** ZSHTAGRESULTMO columns include: ZARTIST, ZTITLE, ZSHAZAMID (for Shazam's own ID), ZTIMESTAMP (Core Data epoch). macOS Shazam app syncs via iCloud when enabled. Group Container path variant: ~/Library/Group Containers/*.group.com.shazam/. The SyncedShazams.csv from the privacy export contains song name, artist, date/time of Shazam, Shazam link. ShazamKit (Apple's developer framework) is for audio recognition in custom apps — not needed here.

#### Plex Media Server — _Local Media Server_

🟡 **Medium — Plex must be installed by the user; not universally present; but for users who have it the local SQLite approach is robust and does not require Plex to be running** · M3; M5 via local API · No system TCC; Plex app must be installed and running (but Trove reads the SQLite directly without requiring Plex running = M3). Full history requires Plex Pass subscription for the dashboard view; raw DB data does not. · effort **M** · 🆕 new

- **Access:** Local API: GET http://localhost:32400/status/sessions/history/all?X-Plex-Token={token} — returns JSON of play history. Local SQLite DB at ~/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db; table metadata_item_settings contains viewOffset, lastViewedAt, viewCount per media item. Plex token: found in Plex Web > Account > Plex.tv > Get the Plex token
- **Recommendation:** Build later — niche audience (self-hosted media enthusiasts), but valuable when present. M3 SQLite read is the right approach; do not require Plex to be running. Jellyfin collector can share the same architecture.
- **Notes:** Plex SQLite: ~/Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db. Key table: metadata_item_settings (view_count, last_viewed_at, view_offset). Media type 1=movie, 2=show, 4=episode, 8=trailer. Plex Pass is needed for dashboard history UI but NOT for raw DB reads. Tautulli (third-party) provides richer history analytics but requires a running service — violates standalone constraint.

#### Jellyfin — _Local Media Server_

🟡 **Medium — Jellyfin is less common than Plex on macOS but growing; free and open-source; local API is well-documented (Swagger at localhost:8096/api-docs/swagger/index.html)** · M5; M3 fallback · Local API key (from Jellyfin admin dashboard); no system TCC · effort **M** · 🆕 new

- **Access:** Local API at http://localhost:8096/Users/{userId}/Items?Filters=IsPlayed&SortBy=DateCreated — returns JSON of watched items. With Playback Reporting plugin installed: /user_usage_stats/ endpoint adds per-session detail. SQLite DB at (varies by install, typically) ~/.config/jellyfin/ or /opt/homebrew/var/jellyfin/ on macOS Homebrew install
- **Recommendation:** Build later alongside Plex — same audience, similar effort. Local API approach preferred over SQLite since the DB path varies by install method. The Playback Reporting plugin provides richer history but is not guaranteed to be installed.
- **Notes:** Without the Playback Reporting plugin, Jellyfin only tracks watched/unwatched and resume position — no timestamped play history. The native /System/ActivityLog/Entries endpoint provides server events including plays. Self-hosted users who run Jellyfin are technical and likely to appreciate Trove; build both Plex and Jellyfin together.

#### YouTube (watch history) — _Video Platform_

🟢 **High — official export, reliable, comprehensive history; same Takeout path as YouTube Music** · M1 · Google account login for Takeout; no special permissions · effort **S** · 📋 planned

- **Access:** Google Takeout: takeout.google.com — select 'YouTube and YouTube Music' only, click 'Multiple formats', switch History from HTML to JSON, export. File: Takeout/YouTube and YouTube Music/history/watch-history.json — array of {title, titleUrl, subtitles (channel), time} objects
- **Recommendation:** Build now (per planned status) — easy M1 import; the JSON is straightforward. Combine with YouTube Music Takeout in same import flow since they come from the same Takeout archive.
- **Notes:** JSON fields: title (video title), titleUrl (youtube.com/watch?v=..., extract video ID), subtitles[0].name (channel name), time (ISO 8601 timestamp). No duration or watch percentage. Ads watched also appear — filter by titleUrl containing 'youtube.com/watch'. Shorts appear with '/shorts/' in URL. One Takeout covers all history since account creation. YouTube Music watch history is a separate file in the same archive: history/music-history.json with similar schema.

#### YouTube Music — _Music Streaming_

🟢 **High — official export path via Google Takeout; straightforward JSON** · M1 · Google account login for Takeout · effort **S** · 🆕 new

- **Access:** Google Takeout: same archive as YouTube watch history — file Takeout/YouTube and YouTube Music/history/music-history.json; also music library data if opted in. Fields: title (song), subtitles[0].name (artist), time (ISO 8601 timestamp). No album or duration in export.
- **Recommendation:** Build now alongside YouTube history — zero marginal effort if Takeout importer already handles YouTube; same archive, same JSON schema. Note that YouTube Music has no official API for listening history.
- **Notes:** Fields are sparse (no album, no duration, no ms_played). History is complete back to account creation. No live API for listening history — Takeout is the only path. For ongoing capture, consider recommending users configure Last.fm scrobbling from YouTube Music via browser extension (e.g., Web Scrobbler).

#### Tidal — _Music Streaming_

🟡 **Medium — favorites/playlists accessible via official API; listening HISTORY is not available via any official endpoint as of 2026. GDPR export is the only path to historical play data.** · M5; M1 fallback · OAuth 2.1 PKCE; developer account at developer.tidal.com · effort **M** · 🆕 new

- **Access:** Official developer API: developer.tidal.com — OAuth 2.1 with PKCE; GET /v2/users/me/favorites/tracks returns liked tracks. CRITICAL: as of 2026 community discussions, Tidal does NOT expose a listening history endpoint — only favorites/playlists. GDPR data request via tidal.com/account/privacy for export.
- **Recommendation:** Spike first — confirm whether GDPR export includes per-play timestamps before building. Favorites/playlists are buildable now but less valuable than history. For Tidal users the better recommendation is to enable Last.fm scrobbling in the Tidal desktop app and collect via Last.fm.
- **Notes:** GitHub discussion (tidal-music/discussions#10) confirms no 'current/recent plays' endpoint exists in the official API. The developer portal is legitimate and the API is functional for catalog and user favorites. PKCE flow works fine for desktop app. Tidal desktop app has built-in Last.fm scrobbling — recommend enabling this as the primary Trove collection path for Tidal users.

#### Deezer — _Music Streaming_

🟡 **Medium — official API exists with user history endpoint; Deezer is popular in Europe and less common in North America; timestamp recovery for full history is reportedly still in development (Deezer support page)** · M5 · OAuth; developer app at developers.deezer.com · effort **M** · 🆕 new

- **Access:** Official API: developers.deezer.com; user.history endpoint: GET https://api.deezer.com/user/me/history with OAuth access_token — returns recently played tracks. OAuth: https://connect.deezer.com/oauth/auth.php flow. Rate limits apply. Also: GDPR export from deezer.com/account
- **Recommendation:** Build later — valid official API path; lower priority given smaller macOS/North American user base. The user.history endpoint covers recent plays; full timestamped history may require GDPR export which Deezer support says is still being improved.
- **Notes:** API endpoint GET /user/me/history returns tracks list with basic metadata. No 'extended streaming history' equivalent to Spotify's GDPR export confirmed as of mid-2026 — Deezer support indicates timestamp recovery for full history is planned. OAuth flow requires server-side redirect URI or use implicit grant for desktop. Deezer also supports Last.fm scrobbling natively in its apps.

#### Snipd — _Podcast Player_

🟡 **Medium — no API; export to Obsidian vault (a watched folder) is the most automatable path; requires user to configure Snipd to sync to a vault folder that Trove also watches** · M2; M1 fallback · None for Obsidian/file-based export; requires Snipd premium for full transcript export · effort **M** · 🆕 new

- **Access:** No public API. Export via: Snipd app > Profile > Export snips — exports to Markdown, Obsidian vault, Readwise, Notion, or Logseq. Individual snip Markdown files contain: episode title, podcast name, timestamp in episode, AI-generated summary, transcript excerpt, user notes. No direct file-drop export to arbitrary folder via standard UI.
- **Recommendation:** Build later — niche but high-signal source for podcast notes/transcripts rather than pure listening history. The M2 approach (Snipd syncs to Obsidian vault, Trove watches that path) is viable but requires user setup. Most valuable for Obsidian users already in Trove's planned scope.
- **Notes:** Snipd exports Markdown with frontmatter including podcast:, episode:, timestamp:, tags:, summary: and a transcript: block. The Obsidian sync plugin (official Snipd integration) writes one .md file per snip to a configurable vault folder. Readwise export is also available. No playback history (only clipped moments). Premium required for full episode transcripts. This is more of a 'highlights/notes' source than a listening history source.

#### Amazon Prime Video — _Video Streaming_

🟡 **Medium — data request technically available but buried in Amazon's privacy portal; format is CSV with good fields including duration. No public API for viewing history.** · M1 · Amazon account login; no special permissions · effort **S** · 🆕 new

- **Access:** GDPR/privacy data request: amazon.com > Account & Lists > Your Account > Request your personal information > select 'PrimeVideo.WatchHistory' — ZIP download with CSV files including title, device, datetime, watch duration (in seconds). Typically ready within minutes to hours. Also: browser console script at primevideo.com/settings/watch-history can scrape visible history.
- **Recommendation:** Build now — minimal engineering effort once Netflix CSV importer exists; the Amazon data request CSV is richer than Netflix's quick export (includes watch duration). Guide users through the data request flow in-app.
- **Notes:** Data request path: amazon.com/gp/help/customer/display.html?nodeId=TP1zlemejtTn6pwYKS or Amazon Privacy Portal. Contains: Title, Device, Country, WatchedStartTime, WatchedEndTime, SecondsWatched. Format can vary slightly by region (EU vs US). Processing time is minutes to 30 days depending on Amazon's queue; typically fast. Data link expires. The 'Watch History' page at primevideo.com/settings/watch-history only shows recent items and requires a browser console script to extract — less reliable than the formal data request.

#### Simkl — _TV/Film/Anime Aggregator_

🟢 **High — clean API, PKCE flow suitable for desktop apps, free tier, comprehensive watch history including anime** · M5 · OAuth 2.0 PKCE; free developer app registration at simkl.com · effort **S** · 🆕 new

- **Access:** REST API: api.simkl.org; GET /sync/all-items/watched — returns full watch history for movies, shows, anime with watched_at timestamps. OAuth 2.0 PKCE flow; client_id from simkl.com/settings/developer. New docs at api.simkl.org (Apiary being sunset Oct 2026).
- **Recommendation:** Build later — Simkl is growing as a Trakt alternative, especially for anime fans. Very similar to Trakt integration; low marginal effort if Trakt is already built. Check if user base warrants both vs. one.
- **Notes:** Simkl supports scrobbling from Plex, Kodi, VLC, Emby, web browsers. Full watch history via /sync/all-items endpoint. Also supports watchlist and ratings. PKCE flow documented at api.simkl.org. Simkl is a good option for anime-heavy users since it has better AniDB/AniList ID mapping than Trakt. New docs supersede Apiary (frozen 2026-05-22).

#### MediaRemote (Universal Now-Playing) — _OS-Level Now Playing_

🟡 **Medium — macOS 15.4 entitlement change is a real blocker for the direct approach; Perl-adapter workaround in mediaremote-rs crate is functional but architecturally fragile (depends on system Perl having entitlement; could change in future macOS)** · M4 · No TCC required; but macOS 15.4+ requires the Perl-adapter workaround or JXA scripting bridge — cannot call MediaRemote directly from unsigned/non-entitled binary · effort **M** · 📋 planned

- **Access:** macOS private framework MediaRemote.framework — subscribe to MRMediaRemoteGetNowPlayingInfo() for real-time now-playing metadata from any app. CRITICAL CAVEAT: macOS 15.4 (2025) restricted framework to entitled Apple processes only. Workaround: mediaremote-rs Rust crate (lib.rs/crates/mediaremote-rs) uses /usr/bin/perl adapter — Perl binary has the entitlement; Rust spawns Perl subprocess that loads a compiled dylib calling MediaRemote, returns JSON via stdout. AppleScript/JXA alternative also works (osascript).
- **Recommendation:** Spike first — evaluate mediaremote-rs Perl adapter on current macOS before committing. The JXA/osascript path (osascript -e 'tell application "System Events" to ...' is simpler but may not yield all metadata. The Biome-based iPhone Now Playing is already built and covers the mobile use case. MediaRemote is needed for Mac-native now-playing (what's playing in Spotify desktop, Music app, etc.)
- **Notes:** mediaremote-rs and mediaremote-adapter are two separate Rust crates implementing the Perl workaround. The perl binary at /usr/bin/perl has com.apple.perl5 entitlement allowing MediaRemote access. The crate compiles a dylib, extracts it to a temp dir at runtime, calls Perl via Command::new('/usr/bin/perl'). Returns JSON with title, artist, album, duration, elapsed, playback rate, app bundle ID. This would complement the existing Biome-based iPhone Now Playing and Apple Music scrobbler. BetterTouchTool community confirmed the 15.4 breakage and that /usr/bin/perl workaround resolves it.

#### Disney+ / Hulu / HBO Max (Max) — _Video Streaming_

🟠 **Low — no official documented export format; privacy portal requests are slow (up to 30 days), format is undocumented and variable, link expiration is short; Disney+/Hulu profile integration announced in 2026 may shift export options** · M1 · Account login at privacy portals; no special permissions · effort **M** · 🆕 new

- **Access:** No public API. Privacy data requests available via OneTrust portals: HBO Max at privacyportal.onetrust.com (linked from hbomax.com/privacy); Disney+ via disneyplus.com/privacy; Hulu via hulu.com/privacy. These GDPR/CCPA requests may return watch history in JSON or CSV — format and completeness varies by service and region. Timeline: up to 30 days.
- **Recommendation:** Icebox — the privacy request path is too unreliable (variable format, slow, expiring link) to build a first-class importer. Monitor whether Disney+ formalizes exports in their Hulu integration rollout (2026). Consider supporting via Trakt instead (users who scrobble these services to Trakt).
- **Notes:** HBO Max privacy portal is at privacyportal.onetrust.com (OneTrust-hosted). Disney+ and Hulu are being integrated in 2026 but neither has a documented watch history export. Some users report receiving detailed JSON from HBO Max privacy requests; others report minimal data. No confirmed stable CSV format for any of these three services. The Trakt path (users configuring browser extensions to scrobble Disney+/Hulu/HBO to Trakt) is more reliable for ongoing capture.

#### TV Time — _TV Tracking_

🟡 **Medium — Chrome extension approach works but is fragile (internal API can change); GDPR request is unreliable in format; TV Time has been losing users to Trakt and Simkl** · M1 · TV Time account login for browser extension; GDPR request via app · effort **M** · 🆕 new

- **Access:** No official API or export. Workarounds: (1) Chrome extension 'TV Time Out' replays the same internal API calls the browser makes — exports all series/episodes with watch status, watched dates, watch counts as JSON or CSV. (2) GDPR data request via app settings — format and completeness vary. The internal API at tvtime.com uses REST endpoints that the browser extension reverse-engineers.
- **Recommendation:** Icebox — fragile extraction paths, declining user base. Users who care about their TV data should be migrated to Trakt via Simkl (which supports TV Time import). Build Trakt and Simkl instead.
- **Notes:** TV Time Liberator and TV Time Out extensions extract history client-side with no server communication. The extracted data includes: series name, episode, season, watched date. These extensions require user to run Chrome, authenticate, and export — cannot be automated by Trove itself without a browser automation dependency. TV Time's declining position in the market makes this low-priority.

#### SoundCloud — _Music Streaming_

🟠 **Low — SoundCloud activities API is social (likes/reposts) not passive playback history; no 'recently listened' endpoint; developer app approvals are slow or blocked for new registrants; service has had financial instability** · M5 · OAuth 2.0; developer app registration required (approval not guaranteed) · effort **L** · 🆕 new

- **Access:** Official API: developers.soundcloud.com — GET /me/activities, /me/activities/tracks (OAuth 2.0 required). Returns user's activity stream including likes, plays, reposts. No documented 'listen history' — activities are social actions, not passive plays. API app registration at soundcloud.com/you/apps; new app registrations reportedly difficult to get approved.
- **Recommendation:** Icebox — no meaningful listening history endpoint; developer app approval is uncertain; SoundCloud's primary value (independent music discovery) is not served by activity-only data. Skip unless a reliable history endpoint emerges.
- **Notes:** SoundCloud's /me/activities endpoint returns a mix of social actions, not passive listening. There is no equivalent to Spotify's recently-played. SoundCloud does support Last.fm scrobbling via the SoundCloud/Last.fm integration — recommend that path for users who want SoundCloud in their listening history. The SoundCloud API developer registration portal has been described as difficult to navigate with low approval rates as of 2025-2026.

#### Pandora — _Music Radio_

🔴 **Blocked — developer API closed to new applicants; no official export; Pandora's market share has declined sharply (radio-style, not on-demand); privacy request path is undocumented** · M1 · Account login for privacy request · effort **L** · 🆕 new

- **Access:** No public API (developer API access closed to new applicants). Station thumb ratings visible in app but not exportable. No official data download. Historical listening data theoretically available via privacy request (privacy@pandora.com) but no documented format. Third-party tool: Soundiiz can export Pandora playlists/thumbs to CSV but requires a paid Soundiiz account.
- **Recommendation:** Skip — closed API, no meaningful export, shrinking relevance. Pandora is radio-style with limited on-demand features; even if exportable, the data (thumbed stations, not individual track history) has low precision.
- **Notes:** Pandora's developer portal explicitly states API access is limited and not accepting new requests. The service is fundamentally radio-style — it does not expose a 'you listened to X song at Y time' history the way streaming services do. Thumb ratings and station history are the only user-specific data. Declining user base post-SiriusXM acquisition.

#### Bandcamp — _Music Purchase_

🟡 **Medium for purchase history (Chrome extension works); Low for listening history (does not exist as a concept in Bandcamp — you own files and play them locally)** · M1 · Account login for Chrome extension scrape · effort **M** · 🆕 new

- **Access:** No listening history API. Official API (bandcamp.com/developer) is for seller/label accounts only (sales reports). Purchase history: visible at bandcamp.com/purchases — a Chrome extension (github.com/rxdazn/bandcamp-purchase-history) extracts it as CSV via DOM scraping. No official export for buyers.
- **Recommendation:** Build later (purchase history only, not listening history) — Bandcamp's value to Trove is as a music purchase/ownership record, not a listening history source. A simple M1 import of the purchase CSV (artist, album, date purchased, price, format) adds meaningful context to a music library. The Chrome extension approach requires user to run the extension and export; consider guiding them through it.
- **Notes:** Bandcamp purchase CSV (from extension): artist, album, date, price, format (MP3/FLAC/etc.). Downloaded files go to wherever the user stores them. No listening history because Bandcamp-purchased music is played in local apps (Music, VLC, etc.) — those plays are captured by Apple Music scrobbler or Last.fm. The real value is ownership metadata: 'I own this album' vs. 'I streamed this album'.

#### Navidrome / Subsonic (self-hosted music) — _Local Music Server_

🟡 **Medium — niche audience (self-hosted music server users); Navidrome 0.59+ has native history; Subsonic API is stable and well-documented; very similar architecture to Plex/Jellyfin** · M3; M5 via local API · Local API credentials (username/password); no system TCC for API; FDA for direct SQLite access · effort **M** · 🆕 new

- **Access:** Subsonic REST API: GET http://localhost:4533/rest/getNowPlaying?v=1.16.1&c=trove&u=USER&p=PASS — returns currently playing. Navidrome 0.59+ stores native scrobble history in its SQLite DB at ~/.config/navidrome/navidrome.db (table scrobble_data). Subsonic getAlbumList2 with type=recent returns recently played albums.
- **Recommendation:** Build later — valuable for audiophile/self-hosted users who have a local music library. Since Navidrome also scrobbles to Last.fm and ListenBrainz, those aggregators may already capture this data. Direct integration adds value for users who don't scrobble to external services.
- **Notes:** Subsonic API also supports scrobble endpoint — could be used to push plays FROM Trove TO Navidrome's history if needed. Navidrome's SQLite DB path on macOS (Homebrew install): ~/Library/Application Support/navidrome/navidrome.db or ~/.config/navidrome/navidrome.db. getNowPlaying, getRecentlyPlayed are standard Subsonic endpoints. OpenSubsonic extensions add richer data. Also covers other Subsonic-compatible servers: Airsonic-Advanced, Funkwhale, Ampache.

### Media: Music, Podcasts, Video & TV — cross-cutting notes

UNIVERSAL AGGREGATORS FIRST: Last.fm and Trakt.tv are the highest-leverage single builds in this domain — each acts as a hub that absorbs listening/watching data from dozens of other services. A user who already scrobbles Spotify/Tidal/Deezer to Last.fm gets all those services for free once the Last.fm collector is built. Similarly, a Trakt user who has Plex, Infuse, or browser scrobbling configured gets Netflix, Disney+, HBO and others without Trove needing per-service integrations. Prioritize these two before building individual streaming service connectors.

SHARED M1 IMPORTER PATTERN: Netflix CSV, Amazon Prime Video CSV, IMDb CSV, Letterboxd ZIP, Spotify GDPR JSON, YouTube Takeout JSON, Bandcamp CSV all follow the same M1 pattern — user drops a file, Trove parses it. These should share a single generic import pipeline with per-format parsers. The same drag-drop UI used for Apple Health export.zip is directly reusable.

MACOS 15.4 MEDIAREMOTE BREAKAGE: The macOS 15.4 entitlement restriction on MediaRemote.framework is a real blocker for the planned universal now-playing spike. The Perl-adapter workaround (used by mediaremote-rs crate) is the current best option but is architecturally fragile (depends on /usr/bin/perl retaining its com.apple.perl5 entitlement). A JXA/osascript fallback exists. This should be spiked before committing engineering time, and the Biome-based iPhone Now Playing (already built) covers the most important mobile use case.

PODCAST DEPTH ORDERING: For podcasts the quality hierarchy is: Overcast extended OPML (best — per-episode timestamps) > Apple Podcasts SQLite (built — play count + last played only) > Pocket Casts unofficial API (fragile — 100 items, no timestamps) > Castro (SQLite only via support access). For users not on Overcast, recommending they enable Overcast's free tier just for history export is a valid in-app suggestion.

SPOTIFY FEBRUARY 2026 API LOCKDOWN: The Spotify API changes materially affect third-party apps but are workable for Trove's personal-use case. The recently-played endpoint survives. The key constraints are: (1) requires Spotify Premium for developer accounts as of Feb 2026, (2) dev-mode limited to 5 users, (3) extended access (>5 users) requires 250k MAU + registered business. For Trove as a personal-use local app, the user authenticates their own account and is within the 5-user limit. Long-term, if Trove is distributed publicly, it needs the extended access review OR should lean on the GDPR export (M1) as the primary path and use the API only for live polling.

STREAMING VIDEO API DESERT: Disney+, Hulu, HBO Max/Max, and Paramount+ have no public APIs and unreliable privacy exports (slow, variable format, expiring links). For these services, Trakt-based scrobbling is the recommended ongoing capture path. For historical backfill, guide users through the formal privacy data requests. No standalone technical path exists.

SHARED OAUTH ARCHITECTURE: Spotify, Tidal, Deezer, Trakt, Simkl, Last.fm (write), and the Apple Music API all use OAuth 2 / PKCE flows. Trove already has an OAuth foundation (built for TickTick and Oura). These should all reuse the same token-store, refresh, and UI flow. Compiled-in app credentials (per docs/oauth-distribution.md) apply to all of them.

---

## Media: Books, Reading & Gaming

This domain covers reading (ebooks, audiobooks, library borrows, highlights) and gaming (PC, console, retro, board games, chess). Access quality varies sharply: Steam's Web API is the gold standard — free, keyless registration, rich data — while Sony PSN and Microsoft Xbox require unofficial reverse-engineered auth but are stable in 2026. Reading data is split between local files (Kindle My Clippings.txt, Apple Books SQLite), cloud APIs (Readwise, Hardcover, Literal), and account exports (Goodreads CSV, StoryGraph CSV, OverDrive email-CSV). Audible lacks an official API but has a well-maintained unofficial Python library (mkb79/Audible) with listening-position support. Nintendo Switch playtime is behind a fragile reverse-engineered Parental Controls auth that requires a relay service, making it a spike/later. Overall the domain is highly feasible for Trove — most high-value sources have clear, privacy-safe local or API paths.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Steam | Gaming — PC | M5 | API key (free self-registration; no OAuth flow needed) | S | 🟢 High — fully documented, stable, free, no scraping. User's profile must be set to public or key must be owner's key. | 🆕 new |
| Readwise | Reading — Highlights Hub | M5 | API token (user retrieves from their account page; no OAuth needed for personal use) | S | 🟢 High — official, documented, stable API. Aggregates Kindle, Apple Books, web articles, PDFs — high leverage single integration. | 🆕 new |
| Kindle Highlights (My Clippings.txt) | Books — Kindle Device | M1 | none (user manually connects Kindle via USB and imports file) | S | 🟢 High — plain text file, stable format, fully parseable. Device-only limitation: Kindle app highlights do NOT appear here. | 📋 planned |
| Goodreads | Books — Social Reading | M1 | none (user-initiated export from their account) | S | 🟢 High — export stable, API deprecated 2020. CSV is comprehensive for library/ratings/shelves. | 🆕 new |
| StoryGraph | Books — Social Reading | M1 | none (user-initiated from account settings) | S | 🟢 High — officially supported export, stable. | 📋 planned |
| Hardcover | Books — Social Reading | M5 | API token (personal, from account settings) | S | 🟢 High — official GraphQL API, documented at docs.hardcover.app, actively developed in 2026. | 🆕 new |
| Literal | Books — Social Reading | M5 | API token / session auth (user credentials required) | M | 🟡 Medium — GraphQL is accessible but API is not officially documented. May change without notice. | 🆕 new |
| Audible | Books — Audiobooks | M5 | Audible account credentials (handled via mkb79/Audible auth flow) | M | 🟡 Medium — unofficial API, well-maintained (updated Jan 2026), returns rich data including last listening position. Risk: Amazon could break it. | 🆕 new |
| Libby / OverDrive | Books — Library Borrows | M1 | none (user-initiated email export from their library account) | S | 🟡 Medium — export available but requires manual trigger (email to self then import). No programmatic pull. | 🆕 new |
| Apple Books (local DB) | Books — Local Reading | M3 | Full Disk Access (FDA) — Containers/ is protected | S | 🟢 High — SQLite files are stable and well-documented by the community. FDA already used by other Trove collectors. | 📋 planned |
| PlayStation Network (PSN) | Gaming — Console | M5 | NPSSO token (user manually retrieves from browser cookie; refresh token valid ~2 months) | M | 🟡 Medium — stable in May 2026 per psnleaderboard.com status page; unofficial but widely used. Risk: Sony could require app-based auth. | 🆕 new |
| Xbox / Xbox Live (OpenXBL) | Gaming — Console | M5 | OpenXBL API key (free tier: 150 req/hour; user registers at xbl.io with Microsoft account) | M | 🟡 Medium — unofficial proxy service (xbl.io) rather than direct Microsoft API; stable and documented. Risk: service dependency (xbl.io could shut down or change pricing). | 🆕 new |
| RetroAchievements | Gaming — Retro | M5 | API key (free, from RA account settings) | S | 🟢 High — official, well-documented, actively maintained JS library (@retroachievements/api on npm). Rust implementation straightforward as plain HTTP. | 🆕 new |
| BoardGameGeek (BGG) | Gaming — Board Games | M5 | none for public profiles; BGG account credentials for private data | S | 🟢 High — stable XML API2, extensively documented, keyless for public data. | 🆕 new |
| Chess.com | Gaming — Chess | M5 | none (public API, no authentication required) | S | 🟢 High — keyless, official, stable REST API returning JSON with embedded PGN. | 🆕 new |
| Lichess | Gaming — Chess | M5 | none for public games; OAuth token for private/rated-only games | S | 🟢 High — official, open-source project, excellent API, NDJSON stream is efficient for large histories. | 🆕 new |
| GOG Galaxy (local DB) | Gaming — PC (DRM-free) | M3 | Full Disk Access (FDA) | M | 🟡 Medium — SQLite confirmed, schema documented by community (GOG-Galaxy-Export-Script on GitHub). macOS exact path needs verification (Windows path well-known, macOS less documented). GOG Galaxy must have been installed and run at least once. | 🆕 new |
| Nintendo Switch (via nxapi Parental Controls) | Gaming — Console | M5 | Nintendo account credentials + nxapi-auth.fancy.org.uk relay service dependency | L | 🟠 Low — requires external relay service (nxapi-auth.fancy.org.uk) to generate auth tokens, violating Trove's standalone constraint. Relay server runs Nintendo Switch Online app on Android; user must trust it. Nintendo has no official API. | 🆕 new |
| Epic Games Store | Gaming — PC | M3 | Full Disk Access (FDA) — only via GOG Galaxy DB path | M | 🟠 Low — no direct API or export. Only reachable indirectly via GOG Galaxy's aggregation DB if user has both installed. | 🆕 new |
| Discord (Game Activity) | Gaming — Cross-platform | M1 | none (user-initiated GDPR-style data export from Discord settings) | S | 🟡 Medium — export available but activity JSON format is sparse (game name, timestamps but no playtime totals). API does not expose personal activity history for third-party apps. | 🆕 new |

### Detail

#### Steam — _Gaming — PC_

🟢 **High — fully documented, stable, free, no scraping. User's profile must be set to public or key must be owner's key.** · M5 · API key (free self-registration; no OAuth flow needed) · effort **S** · 🆕 new

- **Access:** Steam Web API: https://api.steampowered.com/IPlayerService/GetOwnedGames/v1/ (playtime, library), /ISteamUserStats/GetPlayerAchievements/v1/ (achievements per appid), /ISteamUserStats/GetUserStatsForGame/v2/ (stats). Free API key at steamcommunity.com/dev — requires only a Steam account + domain.
- **Recommendation:** Build now — highest value gaming source with the cleanest API in the domain.
- **Notes:** GetOwnedGames returns playtime_forever (minutes) and playtime_2weeks for all owned games. GetPlayerAchievements requires iterating per appid; for large libraries batch with GetSchemaForGame. Profile privacy: if querying with own key, private profiles still work for the owner. No rate limit published but practical cap ~100k req/day. Rust: reqwest + serde suffices, no SDK needed.

#### Readwise — _Reading — Highlights Hub_

🟢 **High — official, documented, stable API. Aggregates Kindle, Apple Books, web articles, PDFs — high leverage single integration.** · M5 · API token (user retrieves from their account page; no OAuth needed for personal use) · effort **S** · 🆕 new

- **Access:** REST: GET https://readwise.io/api/v2/export/ (full highlights + books with updatedAfter cursor for incremental sync). Auth token from readwise.io/access_token. Reader documents: GET https://readwise.io/reader_api (separate token, same account).
- **Recommendation:** Build now — best single integration for reading highlights; aggregates sources that are individually hard (Kindle) into one clean pull.
- **Notes:** Export endpoint returns Book + Highlight records with full metadata (title, author, category, source, tags, notes, timestamps). Rate limited to 20 req/min on export endpoint, 240 req/min otherwise. Incremental via updatedAfter ISO-8601 param. Reader API separate token but same account; returns documents with highlights and location data. Output to vault: reading/readwise/YYYY-MM.jsonl. Covers Kindle highlights that lack a native Amazon API.

#### Kindle Highlights (My Clippings.txt) — _Books — Kindle Device_

🟢 **High — plain text file, stable format, fully parseable. Device-only limitation: Kindle app highlights do NOT appear here.** · M1 · none (user manually connects Kindle via USB and imports file) · effort **S** · 📋 planned

- **Access:** Physical Kindle e-reader USB mount: /Volumes/<KindleName>/documents/My Clippings.txt — plain text, one clipping per block separated by '========='. Also: browser scrape of read.amazon.com/notebook (unofficial, no public API).
- **Recommendation:** Build now — straightforward M1 import; Readwise covers the same data for users who use Readwise, but My Clippings.txt is the zero-dependency fallback.
- **Notes:** Format: title line, '- Your Highlight at location X-Y | Added <date>', blank line, highlight text, '=========' separator. Clipping limit: Amazon caps highlights at ~10% of a purchased book; hits this shows 'You have reached the clipping limit' instead of text. Sideloaded books bypass limit. Does NOT sync Kindle app highlights from phone/tablet — only what was highlighted on that specific device. For cloud highlights (purchased books), read.amazon.com/notebook shows all but has no official API — fragile scraping only, not recommended as primary path. Readwise is the pragmatic cloud complement.

#### Goodreads — _Books — Social Reading_

🟢 **High — export stable, API deprecated 2020. CSV is comprehensive for library/ratings/shelves.** · M1 · none (user-initiated export from their account) · effort **S** · 🆕 new

- **Access:** Account export: goodreads.com/review/import → 'Export Library' → CSV download. Fields: Book Id, Title, Author, ISBN, My Rating, Average Rating, Publisher, Binding, Year Published, Original Publication Year, Date Read, Date Added, Bookshelves, Exclusive Shelf, My Review, Spoiler, Private Notes, Read Count, Recommended For, Recommended By, Owned Copies, Original Purchase Date, Original Purchase Location, Condition, Condition Description, BCID.
- **Recommendation:** Build now — simple M1 import, large user base, clean CSV.
- **Notes:** Public API deprecated December 2020; no new developer keys issued. Re-import workflow: user re-exports periodically. No automatic sync possible without brittle scraping. Export does NOT include friend activity or reading progress within books. StoryGraph is the de-facto Goodreads replacement and has its own export.

#### StoryGraph — _Books — Social Reading_

🟢 **High — officially supported export, stable.** · M1 · none (user-initiated from account settings) · effort **S** · 📋 planned

- **Access:** Account export: app.thestorygraph.com → Manage Account → 'Manage Your Data' → 'Export StoryGraph Library' button → CSV. Contains: Title, Author, Read Status, Star Rating, Review, Tags, Formats, plus reading stats.
- **Recommendation:** Build now alongside Goodreads — growing competitor, shares the same M1 import pattern.
- **Notes:** No API. Enhanced stats export (Reading Journal with pages/duration/dates per session) is a requested but unshipped feature as of mid-2026 — only book-level data available. Export covers TBR, reading, and read shelves plus custom tags. Re-export required for incremental updates.

#### Hardcover — _Books — Social Reading_

🟢 **High — official GraphQL API, documented at docs.hardcover.app, actively developed in 2026.** · M5 · API token (personal, from account settings) · effort **S** · 🆕 new

- **Access:** GraphQL API at https://api.hardcover.app/v1/graphql. Auth: token from account settings → 'Hardcover API'. Query user_books for library; query user_book_reads for reading dates/pages; query book_reviews for reviews.
- **Recommendation:** Build now — rising Goodreads alternative with a real API; small extra effort over CSV-only sources.
- **Notes:** Same API the mobile and web apps use — full fidelity. Supports pagination via cursor. Can write back (mark read, add reviews) — not needed for Trove but shows API depth. No documented rate limit as of 2026; be polite with polling interval (hourly is fine).

#### Literal — _Books — Social Reading_

🟡 **Medium — GraphQL is accessible but API is not officially documented. May change without notice.** · M5 · API token / session auth (user credentials required) · effort **M** · 🆕 new

- **Access:** GraphQL API (undocumented but community-confirmed). Base endpoint inferred from app traffic. No official public docs page but the platform is built on GraphQL and community has documented queries for reading status, books, shelves.
- **Recommendation:** Build later — smaller user base than Hardcover/Goodreads; unofficial API risk. Implement after Hardcover.
- **Notes:** Smaller but design-forward competitor to Goodreads. No export option as of 2026. Community reverse-engineering of the GraphQL schema is the only path. Risk: breaking changes without notice. Spike first to validate stable query shapes before building.

#### Audible — _Books — Audiobooks_

🟡 **Medium — unofficial API, well-maintained (updated Jan 2026), returns rich data including last listening position. Risk: Amazon could break it.** · M5 · Audible account credentials (handled via mkb79/Audible auth flow) · effort **M** · 🆕 new

- **Access:** Unofficial internal API via mkb79/Audible Python library (audible.readthedocs.io). Endpoint: GET /1.0/library with response_groups=last_position_heard,product_details,product_attrs returns library with listening position. Auth: username+password or OAuth2 flow handled by library.
- **Recommendation:** Build later — useful for audiobook listeners (library, progress, finish dates) but unofficial API risk. Implement after Readwise/Kindle.
- **Notes:** mkb79/Audible library (AGPL-3.0) supports async/sync. Documented endpoint GET /1.0/library?response_groups=last_position_heard,product_details returns ASIN, title, authors, listening_position (offset + total length in ms), purchase_date. Libation and OpenAudible use similar paths. Note: compiling Python library into Rust binary is impractical — consider an M6 approach (bundled Python script) or reimplement the HTTP auth in Rust. The auth uses a device-registration flow (RSA key exchange) that is documented in the library source.

#### Libby / OverDrive — _Books — Library Borrows_

🟡 **Medium — export available but requires manual trigger (email to self then import). No programmatic pull.** · M1 · none (user-initiated email export from their library account) · effort **S** · 🆕 new

- **Access:** M1: Library's OverDrive website → History → 'Email history' → CSV delivered to email. M2: Libby Timeline shares via share URL that can be scraped. No API.
- **Recommendation:** Build later — useful for library borrowers but M1-only with extra friction (email delivery). Implement as a simple CSV importer reusing Goodreads/StoryGraph import pattern.
- **Notes:** OverDrive email-CSV export available at any library's OverDrive site. Contains title, author, format, borrow/return dates. Not all libraries enable reading history (opt-in by patron). Libby Timeline (libbyapp.com) has a share/export option but output format is a plain URL list, not structured data. No API keys available — Libby does not expose a developer API.

#### Apple Books (local DB) — _Books — Local Reading_

🟢 **High — SQLite files are stable and well-documented by the community. FDA already used by other Trove collectors.** · M3 · Full Disk Access (FDA) — Containers/ is protected · effort **S** · 📋 planned

- **Access:** Local SQLite: ~/Library/Containers/com.apple.iBooksX/Data/Documents/BKLibrary/BKLibrary-1-091020131601.sqlite (filename varies; glob BKLibrary*.sqlite). Key table: ZBKLIBRARYASSET — columns ZTITLE, ZAUTHOR, ZDATEFINISHED, ZDATEADDED, ZREADINGPROGRESS, ZFILESIZE, ZPAGECOUNT. Highlights: ~/Library/Containers/com.apple.iBooksX/Data/Documents/AEAnnotation/AEAnnotation_*.sqlite.
- **Recommendation:** Build now — FDA already granted for iMessage etc; low marginal cost; captures local reading state that Readwise misses for non-Readwise users.
- **Notes:** ZDATEFINISHED stores a Mac absolute time (seconds since 2001-01-01). ZREADINGPROGRESS is 0.0–1.0. AEAnnotation SQLite has highlights (ZANNOTATIONSELECTEDTEXT) and notes (ZANNOTATIONNOTE) with epub CFI locations. Copy-then-read pattern: copy to temp path before reading to avoid locking. Poll on a schedule (daily). Note: Apple Books highlights also sync to Readwise if user connects them — avoid double-import.

#### PlayStation Network (PSN) — _Gaming — Console_

🟡 **Medium — stable in May 2026 per psnleaderboard.com status page; unofficial but widely used. Risk: Sony could require app-based auth.** · M5 · NPSSO token (user manually retrieves from browser cookie; refresh token valid ~2 months) · effort **M** · 🆕 new

- **Access:** Unofficial API via NPSSO cookie auth. Step 1: user logs into PlayStation.com and visits https://ca.account.sony.com/api/v1/ssocookie in browser to get 64-char NPSSO token. Step 2: exchange for access/refresh tokens via psn-api JS library or psnawp Python library. Endpoints: getUserTitlesPlayedList (game list + playtime), getUserTrophiesForSpecificTitle (trophy data).
- **Recommendation:** Build later — valuable for PlayStation gamers; unofficial auth is acceptable given maturity of the libraries. Spike first to confirm NPSSO flow still works.
- **Notes:** psnawp (Python) and psn-api (JS/TS) are the two mature libraries. Self-rate-limit at 300 req/15min to avoid bans. Returns: trophy title list, earned trophies (with timestamps and rarity), played game list with playtime if visible. Note: playtime data is only available for PS5 titles natively; PS4 titles may show as 0 hours depending on API version. Compiling into Rust: implement the HTTP auth flow directly (it is documented in psn-api source) or use an M6 shim.

#### Xbox / Xbox Live (OpenXBL) — _Gaming — Console_

🟡 **Medium — unofficial proxy service (xbl.io) rather than direct Microsoft API; stable and documented. Risk: service dependency (xbl.io could shut down or change pricing).** · M5 · OpenXBL API key (free tier: 150 req/hour; user registers at xbl.io with Microsoft account) · effort **M** · 🆕 new

- **Access:** OpenXBL (xbl.io): REST API wrapping Xbox Live. Auth: sign in with Microsoft account at xbl.io, get API key, include as X-Authorization header. Endpoints: /v2/achievements/title/{titleId} (achievements), /v2/player/titles (game library + playtime), /v2/achievements/player (recent unlocks).
- **Recommendation:** Build later — good coverage for Xbox gamers, but xbl.io dependency is a concern. Alternatively spike direct Xbox Live REST API (documented at MicrosoftDocs/xbox-live-docs) to avoid the dependency.
- **Notes:** Free tier: 150 req/hour (sufficient for personal sync). Paid plans from $5/month for 500 req/hour. Direct Xbox Live REST (docs.microsoft.com/gaming/gdk) requires XSTS token via Xbox Live auth flow — more complex but eliminates xbl.io dependency. The MS REST docs are at github.com/MicrosoftDocs/xbox-live-docs. Returns: achievement unlock dates, gamerscore, playtime per title. GetTitleHistory covers PC (Xbox Game Pass) and console games.

#### RetroAchievements — _Gaming — Retro_

🟢 **High — official, well-documented, actively maintained JS library (@retroachievements/api on npm). Rust implementation straightforward as plain HTTP.** · M5 · API key (free, from RA account settings) · effort **S** · 🆕 new

- **Access:** Official REST API at api.retroachievements.org. Auth: API key from account control panel (retroachievements.org/settings → API Key). Key endpoints: getUserCompletionProgress (all played games + awards), getAchievementsEarnedBetween (time-range query), getUserGameRankAndScore.
- **Recommendation:** Build later — niche but clean API; low effort once Steam is done since the pattern is identical.
- **Notes:** Returns: games played with achievement counts earned/total, unlock timestamps, hardcore vs softcore mode flags, mastery awards. Rate limited (fair use); library implements back-off. API key is per-user from their own account — no app registration needed.

#### BoardGameGeek (BGG) — _Gaming — Board Games_

🟢 **High — stable XML API2, extensively documented, keyless for public data.** · M5 · none for public profiles; BGG account credentials for private data · effort **S** · 🆕 new

- **Access:** BGG XML API2 (no auth for public data, account login for plays). Base: https://boardgamegeek.com/xmlapi2/. Endpoints: /collection?username=X&played=1 (owned/played games), /plays?username=X&page=N (logged play sessions with date, players, location, comments).
- **Recommendation:** Build now — keyless API, unique data source (no other service captures board game session logs), niche but well-defined.
- **Notes:** Plays endpoint returns: date, quantity, duration, game name/id, players, comments. Collection endpoint returns ratings, owned/wishlist/played status, number of plays. API returns XML; parse with quick-xml crate in Rust. Throttle: ~2 req/sec is safe. Username is public — user just provides their BGG username. Private collections need a session cookie (not worth the complexity; most users have public profiles).

#### Chess.com — _Gaming — Chess_

🟢 **High — keyless, official, stable REST API returning JSON with embedded PGN.** · M5 · none (public API, no authentication required) · effort **S** · 🆕 new

- **Access:** Chess.com Published Data API (PubAPI): https://api.chess.com/pub/player/{username}/games/{year}/{month} returns PGN + metadata for all games in a month. /player/{username}/stats for ratings/win-loss. Keyless, public data.
- **Recommendation:** Build now (pair with Lichess) — both are keyless, same implementation effort, covers nearly all online chess players.
- **Notes:** Returns: PGN, time control, result, accuracy score, ECO opening, ratings, start/end timestamps per game. Monthly archive endpoint easy to paginate. No rate limit published but be polite. Username is public input from user. Accuracy score is a Chess.com proprietary metric. PGN can be stored as-is for replay capability.

#### Lichess — _Gaming — Chess_

🟢 **High — official, open-source project, excellent API, NDJSON stream is efficient for large histories.** · M5 · none for public games; OAuth token for private/rated-only games · effort **S** · 🆕 new

- **Access:** Lichess API: https://lichess.org/api/games/user/{username} with query params ?since=<ms>&until=<ms>&max=300. Returns NDJSON stream. Optional OAuth token for private games. Full API at lichess.org/api.
- **Recommendation:** Build now alongside Chess.com — same effort, open-source ethos matches Trove's.
- **Notes:** NDJSON stream: each line is a complete game object with moves, clocks, opening, players, ratings, result. Incremental via 'since' timestamp. OAuth token adds access to private games and higher rate limits. No API key needed for public games. Streaming endpoint avoids pagination issues for large archives.

#### GOG Galaxy (local DB) — _Gaming — PC (DRM-free)_

🟡 **Medium — SQLite confirmed, schema documented by community (GOG-Galaxy-Export-Script on GitHub). macOS exact path needs verification (Windows path well-known, macOS less documented). GOG Galaxy must have been installed and run at least once.** · M3 · Full Disk Access (FDA) · effort **M** · 🆕 new

- **Access:** Local SQLite: ~/Library/Application Support/GOG.com/Galaxy/storage/galaxy-2.0.db (macOS path inferred from Windows pattern C:\ProgramData\GOG.com\Galaxy\storage\galaxy-2.0.db; macOS equivalent under ~/Library/Application Support/GOG.com/Galaxy/). Tables: GamePieces (library), PlayTasks (launch), GameTimeStatistics (playtime per game).
- **Recommendation:** Spike first — verify exact macOS path and schema; straightforward once confirmed.
- **Notes:** GOG Galaxy stores game library + playtime in galaxy-2.0.db. The GOG-Galaxy-Export-Script (github.com/AB1908/GOG-Galaxy-Export-Script) demonstrates the schema: GamePieces joined with GameTimeStatistics gives playtime in minutes. GOG Galaxy integrations (via Python plugin API) also ingest Steam/Epic data into the same DB — reading it gets multi-platform coverage. Requires Galaxy to be installed but does NOT require it to be running at collection time (M3 poll). Copy-then-read to avoid locks.

#### Nintendo Switch (via nxapi Parental Controls) — _Gaming — Console_

🟠 **Low — requires external relay service (nxapi-auth.fancy.org.uk) to generate auth tokens, violating Trove's standalone constraint. Relay server runs Nintendo Switch Online app on Android; user must trust it. Nintendo has no official API.** · M5 · Nintendo account credentials + nxapi-auth.fancy.org.uk relay service dependency · effort **L** · 🆕 new

- **Access:** Unofficial: nxapi CLI (github.com/samuelthomas2774/nxapi) + nxapi-znca-api relay (nxapi-auth.fancy.org.uk). Auth flow: Nintendo account login → ID token → send to relay server (runs NSO app on Android) to generate f-parameter → exchange for access token → access Parental Controls API for playtime data.
- **Recommendation:** Icebox — violates standalone constraint (relay service dependency). Revisit only if a self-hostable relay solution emerges or Nintendo releases an official API.
- **Notes:** The nxapi-znca-api relay is required because Nintendo's auth uses device attestation that can only be generated by running the actual iOS/Android NSO app. The relay processes an ID token (privacy concern). nxapi issues #8 confirms no playtime API exists outside Parental Controls. Monthly play reports are visible in the Parental Controls app but no bulk export. A potential workaround: screenshots of the monthly report + OCR — not worth engineering.

#### Epic Games Store — _Gaming — PC_

🟠 **Low — no direct API or export. Only reachable indirectly via GOG Galaxy's aggregation DB if user has both installed.** · M3 · Full Disk Access (FDA) — only via GOG Galaxy DB path · effort **M** · 🆕 new

- **Access:** No public API for personal library or playtime. Playtime visible in launcher UI (three-dot menu on game → 'You've Played'). No programmatic export. GOG Galaxy integration path: Epic integrates into GOG Galaxy DB if user has Galaxy + Epic plugin installed.
- **Recommendation:** Icebox as standalone; covered incidentally by GOG Galaxy collector if user runs Galaxy with Epic plugin.
- **Notes:** Epic's developer API (dev.epicgames.com) is for publishers/developers, not personal account data. Community attempts to reverse-engineer Epic's graphql backend have been fragile. Epic Games Data Request (via Epic privacy portal) returns account history but not structured playtime CSV. Best path: document that Epic data appears in GOG Galaxy DB for users who have the Epic integration plugin enabled.

#### Discord (Game Activity) — _Gaming — Cross-platform_

🟡 **Medium — export available but activity JSON format is sparse (game name, timestamps but no playtime totals). API does not expose personal activity history for third-party apps.** · M1 · none (user-initiated GDPR-style data export from Discord settings) · effort **S** · 🆕 new

- **Access:** M1: Discord Settings → Privacy & Safety → 'Request all of my Data' → ZIP export → activities/ folder contains JSON files with game activity. Limited to 30-day rolling window for profile display; the data package may contain more history.
- **Recommendation:** Build later — marginal value vs Steam/Xbox/PSN which have richer playtime data. Worth implementing as a one-shot import to capture game activity across all platforms tracked by Discord Rich Presence.
- **Notes:** Discord data package ZIP: activities/games/ contains per-game JSON with session data. Discord's bot API has Presence Intent for real-time activity but requires the user to run a bot in their own server and grant Privileged Gateway Intents — too invasive. The data export is the pragmatic path. Activity history is also limited — Discord prunes old activity. Primarily useful to capture games played on platforms not otherwise tracked.

### Media: Books, Reading & Gaming — cross-cutting notes

1. API key management pattern: Steam, Readwise, RetroAchievements, BGG (keyless), Chess.com (keyless), Lichess (keyless) all use simple HTTP — a shared Rust HTTP client (reqwest) with a per-service credential store in .trove/sync/ covers them all. Steam and Readwise are the two highest-value, lowest-friction sources and should be built together as the template.

2. Incremental sync: All cloud APIs support cursor/timestamp-based incremental pulls (Steam: no built-in cursor but playtime is cumulative; Readwise: updatedAfter param; Lichess/Chess.com: since= timestamp; BGG: page + mindate). Design the collector trait to store a watermark per source in .trove/sync/.

3. M3 local-DB sources (Apple Books, GOG Galaxy): Both need FDA (already granted) and the copy-then-read pattern already established for iMessage. A shared Rust utility for copy-and-open-SQLite avoids code duplication. Apple Books highlights should check if Readwise is also configured to avoid duplicate vault entries.

4. Unofficial/reverse-engineered APIs (PSN, Xbox via OpenXBL, Audible): All carry breakage risk. Implement with a clear error path that disables the integration gracefully (write a status JSONL line, not a hard failure). PSN and Xbox are worth the risk given their user base; Audible auth (RSA device registration) is complex enough to warrant a spike first.

5. Reading aggregators vs raw sources: Readwise is the highest-leverage single integration because it aggregates Kindle highlights (no native Amazon API), Apple Books highlights (duplicates the local DB path for non-FDA users), and web articles. Build Readwise first, then add raw sources (Apple Books local DB, My Clippings.txt) as fallbacks/complements for users not on Readwise.

6. Chess sources (Chess.com + Lichess): Both are keyless and return PGN. Parse PGN with a Rust library (chess-pgn or similar) to extract opening ECO, move counts, accuracy — rich analytical data at zero auth cost.

7. GOG Galaxy as multi-platform aggregator: GOG Galaxy's SQLite DB absorbs Steam, Epic, and GOG data when the user has those integration plugins. A single M3 collector can capture multi-platform PC gaming history for GOG Galaxy users without needing separate Epic integration.

---

## Financial, Spending & Purchases

Trove has already built the hardest part of this domain: SimpleFIN (bank/card aggregation via user-owned credentials), bank-statement CSV import with Copilot Money backfill, and the canonical transaction model with cross-source dedup. The result is a production-quality foundation covering banks, credit cards, and years of history for most users. The next high-value tier is investments/brokerage (Schwab API is official and individual-developer-friendly; IBKR Flex Query is excellent for power users; SnapTrade can cover 30+ brokerages via one API), followed by crypto (Etherscan/Blockstream are keyless public APIs; exchange CSV exports are trivial M1 with no ongoing maintenance). Peer-to-peer payment apps (Venmo, Cash App, Zelle, PayPal) offer CSV exports that arrive via the existing file-import pipeline. Apple Card/Cash/Savings are uniquely gated behind FinanceKit which is currently iOS-only — the Wallet CSV export is the only desktop route and is already supportable by the existing importer. Personal finance apps (YNAB API, Lunch Money API) are excellent bolt-ons for users who already track there. The main blockers in this domain are not technical: they are business/ToS constraints (Plaid/Teller require developer-held keys that can't safely ship in a standalone binary), the iOS-only FinanceKit wall for Apple's own financial products, and the irreversible loss of granular grocery/retail loyalty purchase detail (no export APIs; GDPR data requests return aggregate data only).

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| SimpleFIN Bridge | Bank/Card Aggregation | M5 | OAuth (user-held SimpleFIN token, stored in macOS Keychain) | S | 🟢 High — already built and validated on real data. 7 accounts synced, 90-day rolling window. Simple protocol: claim once, poll GET daily. | ✅ built |
| Bank/Card Statement CSV, OFX, QFX Import | Bank/Card File Import | M1 | none | S | 🟢 High — already built. Supports Chase card, Chase checking, Copilot Money, and generic CSV (header-sniff). OFX/QFX parsing is the next format to add. | ✅ built |
| Copilot Money Export | Personal Finance App Export | M1 | none | S | 🟢 High — already built and validated on 9,044 rows (2020–2026) covering 13 accounts. | ✅ built |
| YNAB | Personal Finance App (API) | M5 | API key (Personal Access Token; user generates in YNAB settings, no Trove app credential needed) | M | 🟢 High — official, stable, well-documented API. Personal Access Token is the simplest auth path for a standalone app (no OAuth dance). Rate limit: ~200 req/hour. Covers budgets, transactions, categories, payees, scheduled transactions. | 🆕 new |
| Lunch Money | Personal Finance App (API) | M5 | API key (user-generated token, no Trove app credential needed) | M | 🟢 High — official well-documented API, active development, strong developer community. Personal access token model (no OAuth app required). Full CRUD on transactions, categories, accounts. | 🆕 new |
| Charles Schwab Brokerage API | Brokerage / Investments | M5 | OAuth (Schwab Developer account; user OAuth-authorizes Trove to their own account — same flow as TickTick/Oura) | M | 🟢 High — official, free, individual-developer-supported. Schwab is one of the largest US retail brokerages. Holdings, balances, and trade history available. Near-real-time positions. | 🆕 new |
| Interactive Brokers Flex Query API | Brokerage / Investments | M5 | API key (Flex Web Service token from IBKR Client Portal) | M | 🟢 High — well-documented, stable, used by many third-party portfolio tools. Covers all account types including options, futures, forex, crypto. Extremely detailed (fills, commissions, wash sales, dividends, cost basis). Can schedule automated delivery. | 🆕 new |
| SnapTrade | Brokerage / Investments Aggregator | M5 | API key (developer keys from SnapTrade dashboard — problematic for standalone distribution) | L | 🟡 Medium — technically excellent (30+ brokerages via one API, official Rust SDK), but same standalone-distribution problem as Plaid: SnapTrade issues developer keys that can't safely ship in a distributed binary. Viable for a Trove-hosted relay or bring-your-own-key mode; also viable as an alternative for server-side setups. | 🆕 new |
| Fidelity Brokerage CSV Export | Brokerage / Investments | M1 | none | S | 🟢 High — straightforward CSV download, largest US retail brokerage by assets. No API for individual consumers. | 🆕 new |
| Robinhood Data Export | Brokerage / Investments | M1 | none | S | 🟡 Medium — CSV export works but limited: no official API, portfolio snapshot requires manual export, and Robinhood intentionally limits data portability. The CSV covers trades and transfers but not real-time positions. | 🆕 new |
| Vanguard CSV Export | Brokerage / Investments | M1 | none | S | 🟢 High — largest US retirement/mutual fund custodian. CSV export is clean and well-structured. | 🆕 new |
| Coinbase Exchange API | Crypto Exchange | M5 | API key (user-generated at Coinbase; read-only, no Trove app credential) | M | 🟢 High — Coinbase is the largest US crypto exchange. Official API with personal keys (no OAuth app needed). CSV export also available as M1 fallback. Both routes provide full transaction history. | 🆕 new |
| Kraken Exchange API + Export | Crypto Exchange | M5 | API key (user-generated at Kraken with Query Ledger Entries permission) | M | 🟢 High — Kraken is the largest US crypto exchange by volume for many asset pairs. Both API and CSV export paths are well-documented. | 🆕 new |
| Ethereum / EVM Blockchain (Etherscan API) | Crypto On-Chain | M5 | API key (free Etherscan account, no payment) | M | 🟢 High — the canonical way to get Ethereum wallet history. Public blockchain; user provides wallet address(es), not private keys. Completely keyless from Trove's perspective once user provides their address + an Etherscan free API key. | 🆕 new |
| Bitcoin Blockchain (Blockstream Esplora API) | Crypto On-Chain | M5 | none | S | 🟢 High — completely keyless public API. No account needed. User provides their Bitcoin address(es). Blockstream states no persistent logging and no tracking. | 🆕 new |
| PayPal Activity Download | Peer-to-Peer Payments | M1 | none | S | 🟢 High — PayPal's CSV activity download is straightforward and covers 7 years. No API needed for the file import path. | 🆕 new |
| Venmo Transaction Export | Peer-to-Peer Payments | M1 | none | S | 🟢 High — CSV export is functional and covers full account history. No API path available for individuals (Venmo consumes Plaid, it does not expose one). | 🆕 new |
| Cash App Export | Peer-to-Peer Payments | M1 | none | S | 🟢 High — CSV export covers full account lifetime. Export is initiated on web, delivered via email. Straightforward CSV preset. | 🆕 new |
| Zelle Transactions | Peer-to-Peer Payments | M1 | none | S | 🟢 High — Zelle transactions are already captured as bank transactions by SimpleFIN and bank CSV imports. No separate Zelle integration needed. | 🆕 new |
| Apple Card / Apple Cash / Apple Savings | Apple Financial Products | M1 | none | S | 🟢 High for CSV import path. The manual monthly CSV export from Wallet or card.apple.com is fully functional. FinanceKit (live sync) is blocked on desktop — iOS only, requires Apple entitlement. | 🆕 new |
| Amazon Order History | Retail Purchases | M1 | none | M | 🟢 High for data quality; Medium for UX (Request Your Data path takes hours/days). No live API. The planned browser-session scraping approach (matching Copilot's technique) is also feasible as an M6 improvement. | 📋 planned |
| Apple App Store / iTunes Purchase History | Digital Purchases | M1 | none | M | 🟡 Medium — the data request approach works but requires waiting for Apple's response and parsing a specific export format. No live API or direct download. | 🆕 new |
| Plaid (bank aggregation, alternative backend) | Bank/Card Aggregation | M5 | API key (developer-held Plaid client_id + secret — cannot safely ship in a distributed standalone binary without a relay server) | XL | 🟠 Low for standalone distribution. Technically excellent (broadest institution coverage, near-real-time, investments support) but structurally incompatible with Trove's standalone constraint: Plaid issues developer keys that ride on a per-developer quota and ToS. Shipping them in the binary exposes them to extraction; a relay server violates local-first. | 🆕 new |
| Teller.io (bank aggregation, alternative backend) | Bank/Card Aggregation | M5 | API key (developer certificate — cannot safely distribute in standalone binary without relay) | XL | 🟠 Low for standalone distribution. Same developer-key problem as Plaid — Teller issues per-developer mTLS certificates. The free 100-connection tier would cover all users of a distributed Trove on the developer's quota (ToS violation). Near-real-time data and direct bank API connections (not screen scraping) are technical advantages. | 🆕 new |
| Monarch Money | Personal Finance App | M6 | Account credentials (username/password for unofficial API — fragile, may break with app updates, ToS risk) | L | 🟠 Low — no official API. Unofficial libraries depend on reverse-engineered GraphQL queries that break with app updates. ToS risk for automated credential use. Monarch has an export feature (Settings → Export Data → CSV) that is the safer path. | 🆕 new |
| Actual Budget | Local Personal Finance App | M3 | none (user's own Actual data directory) | M | 🟡 Medium — Actual is a niche but growing local-first personal finance app with strong overlap with Trove's audience. Local SQLite is directly readable. The database schema is documented at actualbudget.org/docs/contributing/project-details/database/. Migration history is well-documented. | 🆕 new |
| GoCardless Bank Account Data (EU/UK Open Banking) | Open Banking (EU/UK) | M5 | API key (developer-held GoCardless secret — same distribution problem as Plaid for general users) | L | 🟡 Medium for EU/UK users specifically. GoCardless offers a free production tier and covers virtually all European banks through PSD2 mandates. Same standalone-distribution issue: developer-held keys. But the free tier (50 connections) is more workable for BYOK. | 🆕 new |
| Grocery/Retail Loyalty Programs | Retail Purchase Detail | M1 | none | XL | 🟠 Low — data request process is slow, format is inconsistent, and coverage depends on privacy law jurisdiction. No programmatic path. Kroger earned $527M selling shopper data but provides minimal structured export to consumers. | 🆕 new |
| Stripe Billing / Invoice Data | SaaS Billing | M1 | none (CSV export from Stripe dashboard for users with Stripe accounts) | S | 🟡 Medium — only relevant for the subset of users who have a Stripe customer account (freelancers, developers, SaaS subscribers billed through Stripe). Not a mass-market integration. | 🆕 new |
| Crypto Tax Aggregator Exports (Koinly, CoinTracker) | Crypto Tax | M1 | none | S | 🟡 Medium — useful for users who already use these services and want to seed Trove with their normalized crypto history. CSV export is clean. No ongoing sync path. | 🆕 new |
| FinanceKit iOS Companion (Apple Card/Cash live sync) | Apple Financial Products (Live) | M4 | Apple entitlement (requires approval) + user consent in Settings | XL | 🔴 Blocked for the macOS app. Would require an iOS companion app, Apple entitlement approval, and a mechanism to sync data to the Mac vault (iCloud, local network, or USB). Technically feasible as a companion app but significant scope. | 📋 planned |

### Detail

#### SimpleFIN Bridge — _Bank/Card Aggregation_

🟢 **High — already built and validated on real data. 7 accounts synced, 90-day rolling window. Simple protocol: claim once, poll GET daily.** · M5 · OAuth (user-held SimpleFIN token, stored in macOS Keychain) · effort **S** · ✅ built

- **Access:** POST https://beta-bridge.simplefin.org/simplefin/claim with one-time setup token → long-lived access URL; GET {access_url}/accounts?start-date={epoch} for transactions+balances. No per-developer key — each user pays ~$1.50/mo and owns their own credential.
- **Recommendation:** Build now
- **Notes:** The only bank aggregator where each user holds their own credential — this is why it was chosen over Plaid/Teller. ~16k institutions via MX network. 90-day window per sync pass (not a Trove limit; it is the SimpleFIN Bridge's upstream cap — institutions give ~90 days). Deep history is seeded via file import. Apple Card/Cash/Savings are unreachable by any aggregator (FinanceKit-gated). Connection breaks (bank MFA resets) surface in the Bridge dashboard; Trove shows staleness and deep-links there.

#### Bank/Card Statement CSV, OFX, QFX Import — _Bank/Card File Import_

🟢 **High — already built. Supports Chase card, Chase checking, Copilot Money, and generic CSV (header-sniff). OFX/QFX parsing is the next format to add.** · M1 · none · effort **S** · ✅ built

- **Access:** User downloads from bank portal (usually 90-day or custom date-range export): Chase card (CSV via Activity & Orders → Download), Chase checking (CSV same path), Fidelity (CSV; stopped OFX Jan 17 2026), Amex (CSV), BofA (CSV; stopped OFX/QFX Sep 30 2025). OFX/QFX still valid for smaller banks/CUs that have not yet dropped it. Drop file on Trove → `finance/imports/`.
- **Recommendation:** Build now
- **Notes:** File import is a permanent peer backend, not a fallback — it is the only route to Apple Card, Apple Cash, Venmo, Cash App, and deep history past aggregator windows. OFX/QFX unambiguous parse, no column guessing, still offered by many smaller banks/CUs even as majors (BofA, Chase, Fidelity) have dropped it. Per-bank presets added on demand when a user submits their header row. Checking-CSV balance backfill (running balance column) is a noted follow-up for net-worth-over-time.

#### Copilot Money Export — _Personal Finance App Export_

🟢 **High — already built and validated on 9,044 rows (2020–2026) covering 13 accounts.** · M1 · none · effort **S** · ✅ built

- **Access:** Copilot iOS/Mac app → Settings → Account → Export Transactions → CSV. Produces `transactions.csv` with columns: date, name, amount, status, category, parent category, account, account mask, excluded, type, note, recurrings. Web app at copilot.money also has export.
- **Recommendation:** Build now
- **Notes:** Primary day-one history seeder: users who already use Copilot get years of categorized history instantly. Signs are inverted vs. vault convention (spending positive); handled. Cross-source dedup uses relaxed Copilot-specific matching (merchant names differ from bank raw descriptions). Copilot's account mask enables alias adoption so SimpleFIN accounts absorb their Copilot history. Copilot has no API; export-only is confirmed as of 2026.

#### YNAB — _Personal Finance App (API)_

🟢 **High — official, stable, well-documented API. Personal Access Token is the simplest auth path for a standalone app (no OAuth dance). Rate limit: ~200 req/hour. Covers budgets, transactions, categories, payees, scheduled transactions.** · M5 · API key (Personal Access Token; user generates in YNAB settings, no Trove app credential needed) · effort **M** · 🆕 new

- **Access:** REST API at https://api.ynab.com/v1. Endpoints: /budgets, /budgets/{id}/accounts, /budgets/{id}/transactions, /budgets/{id}/categories, /budgets/{id}/months. Personal Access Token: YNAB account → Account Settings → Developer Settings → New Token. No OAuth app required for own-account access.
- **Recommendation:** Build now
- **Notes:** Excellent for users who already track in YNAB — they have years of categorized, merchant-normalized data. The API is read/write but Trove only needs read. Data quality is high because YNAB users manually review every transaction. Vault format: normalize to canonical transaction schema, supplement with YNAB categories and budget metadata in `extra`. Dedup against SimpleFIN transactions using fuzzy matcher. No Trove-held credentials needed (user pastes their own PAT).

#### Lunch Money — _Personal Finance App (API)_

🟢 **High — official well-documented API, active development, strong developer community. Personal access token model (no OAuth app required). Full CRUD on transactions, categories, accounts.** · M5 · API key (user-generated token, no Trove app credential needed) · effort **M** · 🆕 new

- **Access:** REST API at https://lunchmoney.dev/ (v1 stable) and https://alpha.lunchmoney.dev/v2/ (v2 alpha, expected GA 2026). Access token: Lunch Money app → Developers page. GET /v1/transactions, /v1/accounts, /v1/categories. v2 adds typed TypeScript SDK at github.com/lunch-money/lunch-money-js-v2.
- **Recommendation:** Build now
- **Notes:** Best-in-class API among personal finance apps. Actively maintained v2 SDK. Like YNAB, Lunch Money users have years of categorized data. Plaid-connected accounts in Lunch Money provide near-real-time sync that Trove can pull. Has tags, recurring items detection, and budget data — all importable. No Trove-held credentials. Good complement: users who use Lunch Money + Trove get the Lunch Money categorization layer for free.

#### Charles Schwab Brokerage API — _Brokerage / Investments_

🟢 **High — official, free, individual-developer-supported. Schwab is one of the largest US retail brokerages. Holdings, balances, and trade history available. Near-real-time positions.** · M5 · OAuth (Schwab Developer account; user OAuth-authorizes Trove to their own account — same flow as TickTick/Oura) · effort **M** · 🆕 new

- **Access:** Official Individual Developer tier at developer.schwab.com. OAuth 2.0. Endpoints: GET /accounts/{accountNumber}/positions (holdings), GET /accounts/{accountNumber}/transactions (trade history, up to 1 year per request). Free with any Schwab brokerage account. Register app → Individual Developer account (separate login from brokerage). Rust: use reqwest + oauth2 crates.
- **Recommendation:** Build now
- **Notes:** Schwab includes the API free with any brokerage account — no tier/fee. The 'Individual Developer' role explicitly supports personal-account apps. Coverage includes equities, options, mutual funds, ETFs, retirement accounts at Schwab. Complement with Fidelity CSV for users who have both. For the vault: transactions/holdings live in `finance/investments/<account-id>/` as JSONL. OFX export from Schwab also still works as M1 fallback.

#### Interactive Brokers Flex Query API — _Brokerage / Investments_

🟢 **High — well-documented, stable, used by many third-party portfolio tools. Covers all account types including options, futures, forex, crypto. Extremely detailed (fills, commissions, wash sales, dividends, cost basis). Can schedule automated delivery.** · M5 · API key (Flex Web Service token from IBKR Client Portal) · effort **M** · 🆕 new

- **Access:** IBKR Client Portal → Reports → Flex Queries → Create Query (XML or CSV format). Flex Web Service API: Step 1 POST https://gdcdyn.interactivebrokers.com/Universal/servlet/FlexStatementService.SendRequest to get reference code; Step 2 POST to FlexStatementService.GetStatement to download. Token: Client Portal → Settings → Reports & Statements → Flex Web Service token.
- **Recommendation:** Build now
- **Notes:** IBKR is very popular with active traders and international users. Flex Query is the gold standard for brokerage data export — the level of detail (individual fills, lot-by-lot cost basis, corporate actions) exceeds what most aggregators return. The two-step HTTP API is unusual but stable. Can be polled daily. IBKR also offers a CSV export from the portal as M1 fallback.

#### SnapTrade — _Brokerage / Investments Aggregator_

🟡 **Medium — technically excellent (30+ brokerages via one API, official Rust SDK), but same standalone-distribution problem as Plaid: SnapTrade issues developer keys that can't safely ship in a distributed binary. Viable for a Trove-hosted relay or bring-your-own-key mode; also viable as an alternative for server-side setups.** · M5 · API key (developer keys from SnapTrade dashboard — problematic for standalone distribution) · effort **L** · 🆕 new

- **Access:** REST API at docs.snaptrade.com. SDKs: Rust (via snaptrade-sdks on GitHub/passiv/snaptrade-sdks). Free tier: 100 live connections. GET /accounts, /accounts/{accountId}/positions, /accounts/{accountId}/transactions. Developer keys issued per app — same ToS issue as Plaid (developer-held keys ship in binary or require relay).
- **Recommendation:** Spike first
- **Notes:** Covers Robinhood, Schwab, Fidelity, TD, IBKR, and 25+ more through one API. For a fully standalone app, would need bring-your-own-API-key UX (user registers their own SnapTrade developer account — unusual friction) or a Trove relay server. Free tier (100 connections) is generous. SOC 2 Type 2. Good fallback for users whose broker is not directly supported by native APIs. Defer Schwab/IBKR native first, then revisit SnapTrade for broader coverage.

#### Fidelity Brokerage CSV Export — _Brokerage / Investments_

🟢 **High — straightforward CSV download, largest US retail brokerage by assets. No API for individual consumers.** · M1 · none · effort **S** · 🆕 new

- **Access:** fidelity.com → Accounts & Trade → Activity & Orders → History → Download button → CSV. Date range limited to 90-day windows (need multiple downloads for full history). Stopped OFX exports January 17, 2026. Positions: Accounts → Portfolio → Download.
- **Recommendation:** Build now
- **Notes:** Fidelity has no consumer-facing API. CSV is the only programmatic path. The 90-day window limit means users need ~4 CSV files per year for full history. A Fidelity-specific CSV preset (column detection) plus clear onboarding instructions ('download each 90-day window and drop them here') is the full solution. Column format: Action, Settlement Date, Account Number, Security Description, Security Symbol, Quantity, Price, Commission, Amount. Holdings export has a different format.

#### Robinhood Data Export — _Brokerage / Investments_

🟡 **Medium — CSV export works but limited: no official API, portfolio snapshot requires manual export, and Robinhood intentionally limits data portability. The CSV covers trades and transfers but not real-time positions.** · M1 · none · effort **S** · 🆕 new

- **Access:** Account → Settings → Privacy & Security → Download my data → CSV (trade history, transfers). Also: Reports and Statements → Generate Report → Download CSV for specific date ranges. No official API for retail users (unofficial reverse-engineered API exists but is unsupported).
- **Recommendation:** Build now
- **Notes:** Robinhood is extremely popular for US retail investors, especially younger users. Export path is functional but friction-heavy (no date range on the main download; statement-by-statement for older history). SnapTrade covers Robinhood for live sync if the relay issue is solved. For v1, CSV preset + M1 is the right approach. The unofficial Python API (github.com/robin-stocks/robin-stocks) is fragile — not suitable for a production integration.

#### Vanguard CSV Export — _Brokerage / Investments_

🟢 **High — largest US retirement/mutual fund custodian. CSV export is clean and well-structured.** · M1 · none · effort **S** · 🆕 new

- **Access:** investor.vanguard.com → My Accounts → Transaction History → Download → CSV. Date range up to 18 months per download. Holdings: Portfolio → Export. No official consumer API; Plaid can reach Vanguard (Transactions, Assets, Balance products) but requires developer keys.
- **Recommendation:** Build now
- **Notes:** Vanguard dominates 401k/IRA assets. No official API. The 18-month CSV export limit means users need 2 downloads for 3 years of history. A Vanguard-specific CSV preset covers most of their data. 401k accounts at Vanguard are reachable the same way.

#### Coinbase Exchange API — _Crypto Exchange_

🟢 **High — Coinbase is the largest US crypto exchange. Official API with personal keys (no OAuth app needed). CSV export also available as M1 fallback. Both routes provide full transaction history.** · M5 · API key (user-generated at Coinbase; read-only, no Trove app credential) · effort **M** · 🆕 new

- **Access:** Coinbase Developer Platform: https://api.coinbase.com/v2/transactions, /accounts, /orders. Personal API key at coinbase.com → Settings → API. Read-only scope sufficient. Also: CSV export at coinbase.com → Taxes → Generate Report (CSV of all transactions, any date range). REST API uses API key + secret (HMAC-signed requests).
- **Recommendation:** Build now
- **Notes:** As of 2026, Coinbase (and all US crypto exchanges) must issue Form 1099-DA with gross proceeds — they already have full transaction histories. The REST API with personal keys is the cleanest path: user generates a read-only key in their Coinbase settings, pastes it into Trove, and gets ongoing sync. CSV export (from Taxes page) is the M1 fallback for initial backfill or users who prefer not to create API keys. Also covers Coinbase Advanced Trade (formerly Pro).

#### Kraken Exchange API + Export — _Crypto Exchange_

🟢 **High — Kraken is the largest US crypto exchange by volume for many asset pairs. Both API and CSV export paths are well-documented.** · M5 · API key (user-generated at Kraken with Query Ledger Entries permission) · effort **M** · 🆕 new

- **Access:** Kraken REST API: https://api.kraken.com/0/private/TradesHistory (paginated, 50 trades/request), /Ledgers (deposits/withdrawals/fees). API key: kraken.com → Security → API → Add Key (permissions: Query Ledger Entries, Export Data). CSV export: kraken.com profile icon → Documents → Create Export (Ledgers or Trades, CSV format, custom date range). Export processing can take minutes to a week.
- **Recommendation:** Build now
- **Notes:** For ongoing sync use the API; for initial full-history backfill the CSV export is faster (single file vs. paginating through potentially thousands of requests). The export request can take time to generate. Pairs well with the crypto CSV importer. Kraken's API returns up to 50 trades per request with offset-based pagination — manageable in a Rust client. Also covers Kraken staking rewards and earn positions via the Ledgers endpoint.

#### Ethereum / EVM Blockchain (Etherscan API) — _Crypto On-Chain_

🟢 **High — the canonical way to get Ethereum wallet history. Public blockchain; user provides wallet address(es), not private keys. Completely keyless from Trove's perspective once user provides their address + an Etherscan free API key.** · M5 · API key (free Etherscan account, no payment) · effort **M** · 🆕 new

- **Access:** Etherscan API v2: https://api.etherscan.io/v2/api?chainid=1&module=account&action=txlist&address={wallet}. Free API key at etherscan.io/myapikey (no payment). Also covers ERC-20 token transfers (&action=tokentx), internal transactions (&action=txlistinternal). Rate: 5 calls/sec on free tier. As of July 2026, free tier returns max 1,000 records/request (down from 10,000).
- **Recommendation:** Build now
- **Notes:** User inputs their wallet address(es) — no private key ever touches Trove. Etherscan API v2 supports multiple chains (Polygon, Arbitrum, Optimism, Base, etc.) with the same API key via chainid parameter. Free tier is adequate for personal wallet history. For heavy users with large wallets, pagination handles the 1,000-record cap. Also supports ENS name resolution. Privacy note: querying wallet addresses leaks them to Etherscan — document this; power users can self-host Erigon/Geth instead (beyond scope).

#### Bitcoin Blockchain (Blockstream Esplora API) — _Crypto On-Chain_

🟢 **High — completely keyless public API. No account needed. User provides their Bitcoin address(es). Blockstream states no persistent logging and no tracking.** · M5 · none · effort **S** · 🆕 new

- **Access:** No API key required. https://blockstream.info/api/address/{bitcoin_address}/txs (paginated). Also: https://mempool.space/api/address/{addr}/txs. Full transaction history for any Bitcoin address. Blockstream hosts free public Esplora instances; open-source self-hostable.
- **Recommendation:** Build now
- **Notes:** The simplest crypto integration: no credentials at all, just wallet addresses. Blockstream's open-source Esplora can also be self-hosted for users who care about privacy against Blockstream. mempool.space is an alternative with the same REST API format. Covers the full UTXO model (inputs/outputs). Pair with Etherscan for EVM chains to cover ~95% of crypto users. Solana: Helius API covers Solana wallet history but requires a paid plan for `getTransactionsForAddress` (launched Oct 2025) — defer or use Solana's public RPC as M6/fallback.

#### PayPal Activity Download — _Peer-to-Peer Payments_

🟢 **High — PayPal's CSV activity download is straightforward and covers 7 years. No API needed for the file import path.** · M1 · none · effort **S** · 🆕 new

- **Access:** PayPal account → Activity → Download → CSV or TAB format. Date range up to 7 years (max 50k records per file, split/ZIP if larger). URL shortcut: paypal.com/reports/dlog. API: /v1/reporting/transactions (requires OAuth app — developer-keyed, not suitable for standalone distribution). The activity download is the standalone path.
- **Recommendation:** Build now
- **Notes:** PayPal REST Transaction Search API is read-only and covers only 3 years; the activity download covers 7. Since PayPal's REST API requires developer-held app credentials (not user-generated keys), the file import path (M1) is cleanly standalone. A PayPal CSV preset for the existing importer is the full solution. Covers Venmo transfers that settle to/from a linked PayPal balance.

#### Venmo Transaction Export — _Peer-to-Peer Payments_

🟢 **High — CSV export is functional and covers full account history. No API path available for individuals (Venmo consumes Plaid, it does not expose one).** · M1 · none · effort **S** · 🆕 new

- **Access:** Venmo website (account.venmo.com) → Privacy tab → Request Your Data → Transaction History → CSV or JSON. Also: direct URL https://account.venmo.com/api/statement/download?startDate=YYYY-MM-DD&endDate=YYYY-MM-DD&csv=true while logged in. No public API for individuals.
- **Recommendation:** Build now
- **Notes:** Venmo transactions don't flow through SimpleFIN or any aggregator (Venmo's Plaid integration is inbound-only for linking external accounts). The CSV export is the only complete source. Transactions include peer payments (with notes), merchant payments, and balance transfers. A Venmo CSV preset in the existing importer covers this fully. Transactions that settle to a linked bank account will also appear in bank statements, so dedup is relevant.

#### Cash App Export — _Peer-to-Peer Payments_

🟢 **High — CSV export covers full account lifetime. Export is initiated on web, delivered via email. Straightforward CSV preset.** · M1 · none · effort **S** · 🆕 new

- **Access:** cash.app website → Activity → three-dot menu → Export Transactions → All Time → CSV. The export is emailed to the registered email address within minutes. Mobile app has no export function; must use web. No public API.
- **Recommendation:** Build now
- **Notes:** Like Venmo, Cash App is a walled garden for aggregators. The CSV export covers peer payments, Bitcoin purchases/sales (if used), and Cash App Card transactions. Cash App retains full history for active accounts. Email delivery of the CSV (rather than direct download) is a minor friction worth documenting in onboarding. Bitcoin transactions from Cash App overlap with on-chain data from Blockstream API — dedup on txid.

#### Zelle Transactions — _Peer-to-Peer Payments_

🟢 **High — Zelle transactions are already captured as bank transactions by SimpleFIN and bank CSV imports. No separate Zelle integration needed.** · M1 · none · effort **S** · 🆕 new

- **Access:** No standalone Zelle app export. Zelle transactions appear in the bank account that handles them — export via the bank's portal (CSV/OFX) already captured by SimpleFIN or bank CSV import. For standalone Zelle app users: no direct export; must go through bank statements.
- **Recommendation:** Build now
- **Notes:** Zelle has no independent transaction export. All Zelle activity surfaces in the bank account — already covered by SimpleFIN (as bank transactions) and bank CSV imports. The Trove onboarding copy should explain this rather than listing Zelle as a separate integration requiring action. The only gap is users who use only the standalone Zelle app without a connected bank account — an extremely rare configuration.

#### Apple Card / Apple Cash / Apple Savings — _Apple Financial Products_

🟢 **High for CSV import path. The manual monthly CSV export from Wallet or card.apple.com is fully functional. FinanceKit (live sync) is blocked on desktop — iOS only, requires Apple entitlement.** · M1 · none · effort **S** · 🆕 new

- **Access:** Apple Wallet app (iPhone): Wallet → Apple Card → Card Balance → Statements → Export Transactions → CSV/OFX/QFX/QBO (per month). Web: card.apple.com → Statements → Export Transactions. Apple Cash PDF statement: Wallet → Apple Cash → Request Statement (last 12 months to Apple ID email). No API; FinanceKit is iOS-only as of 2026 (requires Apple entitlement + iPhone).
- **Recommendation:** Build now
- **Notes:** Apple Card, Apple Cash, and Apple Savings are unreachable by any desktop aggregator — SimpleFIN cannot connect to them. FinanceKit (the live sync API used by Copilot, Monarch, YNAB on iPhone) requires an Apple-granted entitlement AND runs only on iOS/iPadOS (not macOS as of June 2026). The desktop path is always CSV export: user exports from Wallet (per-month, iPhone or card.apple.com) and drops into Trove. A preset for Apple Card CSV format (columns: Transaction Date, Clearing Date, Description, Merchant, Category, Type, Amount (USD)) should be added when a real export file is available. Apple Cash has only PDF statement (last 12 months) — future: PDF→text extraction. Apple Savings uses the Apple Card CSV export path (same Wallet interface). The existing Copilot import already seeds Apple Card history for Copilot users.

#### Amazon Order History — _Retail Purchases_

🟢 **High for data quality; Medium for UX (Request Your Data path takes hours/days). No live API. The planned browser-session scraping approach (matching Copilot's technique) is also feasible as an M6 improvement.** · M1 · none · effort **M** · 📋 planned

- **Access:** Amazon removed native CSV export March 2023. Current official path: amazon.com → Account → Privacy Central → Request Your Data → Order History → download ZIP (hours to days, email notification). Contains `Retail.OrderHistory.1/` folder with detailed order CSV. Alternative: third-party Chrome extensions (Order History Exporter for Amazon — JSON/CSV) that scrape the orders page in the logged-in browser session.
- **Recommendation:** Build now
- **Notes:** Amazon is the most common retail purchase source for most users. The data request path yields complete order history in CSV format — better than screen scraping. Key fields: Order ID, Order Date, Title, Category, ASIN, Quantity, Payment Instrument Type, Unit Price, Unit Price Tax, Shipping Charge, Total Charged, Tracking Number. Pair with bank/card transactions: match order total ± date window to transaction descriptions containing 'AMZN' or 'Amazon' to enrich transactions with line-item detail. The planned browser-session scraper (using Trove's existing browser snapshot machinery from `browser.rs`) is the v2 path for ongoing enrichment without waiting for the data request.

#### Apple App Store / iTunes Purchase History — _Digital Purchases_

🟡 **Medium — the data request approach works but requires waiting for Apple's response and parsing a specific export format. No live API or direct download.** · M1 · none · effort **M** · 🆕 new

- **Access:** reportaproblem.apple.com → full purchase history visible (App Store, iTunes, Apple TV+, Apple Music, Apple Arcade). Structured CSV export: apple.com/privacy → Data and Privacy → Get a copy of your data → App Store Activity (includes purchase history). No API. Apple sends email with download link (within hours for most users).
- **Recommendation:** Build later
- **Notes:** The Apple Privacy data export includes App Store Activity with purchase history: app name, purchase date, amount, category. Useful for subscription tracking and digital spending analysis. The friction (request → wait → download ZIP → navigate folder structure) is moderate. Worth building an importer for the Apple Privacy export ZIP (which also contains other Trove-relevant data like Maps search history, Siri usage, etc.) as a single M1 integration. Note: iOS App Store receipts (for in-app purchases) are a separate, developer-facing concept — not relevant here.

#### Plaid (bank aggregation, alternative backend) — _Bank/Card Aggregation_

🟠 **Low for standalone distribution. Technically excellent (broadest institution coverage, near-real-time, investments support) but structurally incompatible with Trove's standalone constraint: Plaid issues developer keys that ride on a per-developer quota and ToS. Shipping them in the binary exposes them to extraction; a relay server violates local-first.** · M5 · API key (developer-held Plaid client_id + secret — cannot safely ship in a distributed standalone binary without a relay server) · effort **XL** · 🆕 new

- **Access:** Plaid Link widget embedded in app, REST API calls to https://production.plaid.com. Developer keys issued per developer/company. Free trial: up to 10 production Items; paid plans from ~$0.10–$2.00/Item/month depending on products. /transactions/get, /accounts/get, /investments/holdings/get, /investments/transactions/get.
- **Recommendation:** Icebox
- **Notes:** Plaid is the gold standard for bank data coverage and quality but requires developer-held credentials that cannot be distributed safely in a standalone binary. SimpleFIN solves this structurally (user-held credentials). Plaid is viable only as a bring-your-own-key option (user registers their own Plaid developer account — too much friction for general users) or if Trove ever runs a relay server (violates local-first principle). Same issue applies to MX Technologies, Finicity, Akoya, Yodlee, TrueLayer, SaltEdge — all require developer-held keys for production. Document as 'possible via BYOK for power users' rather than a first-class integration. Plaid Investments is uniquely appealing for brokerage coverage but blocked by the same constraint.

#### Teller.io (bank aggregation, alternative backend) — _Bank/Card Aggregation_

🟠 **Low for standalone distribution. Same developer-key problem as Plaid — Teller issues per-developer mTLS certificates. The free 100-connection tier would cover all users of a distributed Trove on the developer's quota (ToS violation). Near-real-time data and direct bank API connections (not screen scraping) are technical advantages.** · M5 · API key (developer certificate — cannot safely distribute in standalone binary without relay) · effort **XL** · 🆕 new

- **Access:** Teller Connect widget (OAuth flow), REST API at https://api.teller.io/accounts/{id}/transactions. Free tier: 100 live connections (generous for personal use). Mutual TLS authentication using a certificate issued per developer.
- **Recommendation:** Icebox
- **Notes:** Same constraint as Plaid. Teller's technical approach (direct bank API connections, no screen scraping) is cleaner, and their free 100-connection tier sounds generous until you realize 100 connections = 100 total users for the developer. Not viable for a distributed app. BYOK path is possible but same friction problem. Consider as a fallback for institutional/enterprise Trove deployments where a relay is acceptable.

#### Monarch Money — _Personal Finance App_

🟠 **Low — no official API. Unofficial libraries depend on reverse-engineered GraphQL queries that break with app updates. ToS risk for automated credential use. Monarch has an export feature (Settings → Export Data → CSV) that is the safer path.** · M6 · Account credentials (username/password for unofficial API — fragile, may break with app updates, ToS risk) · effort **L** · 🆕 new

- **Access:** No official public API as of June 2026. Unofficial reverse-engineered Python library (github.com/hammem/monarchmoney) and JavaScript API (github.com/pbassham/monarch-money-api) use GraphQL queries against the web app's endpoints. Several MCP servers wrap this for LLM access.
- **Recommendation:** Build later
- **Notes:** Monarch has a CSV export (similar to Copilot) that could be added as an M1 import preset — the safer, ToS-compliant path. The unofficial API is fragile maintenance burden. If Monarch releases an official API, upgrade to M5. Many users are migrating from Monarch to other apps (Lunch Money gaining share per 2026 comparisons). Priority: add Monarch CSV export preset alongside Copilot preset, defer API work unless official API launches.

#### Actual Budget — _Local Personal Finance App_

🟡 **Medium — Actual is a niche but growing local-first personal finance app with strong overlap with Trove's audience. Local SQLite is directly readable. The database schema is documented at actualbudget.org/docs/contributing/project-details/database/. Migration history is well-documented.** · M3 · none (user's own Actual data directory) · effort **M** · 🆕 new

- **Access:** Actual stores data in a local SQLite database. Default path: ~/Library/Application Support/Actual/... (the exact path varies by the self-hosted vs. Electron app variant). Export: Actual UI → Settings → Export Data → .zip containing `db.sqlite`. Official Node.js API package: @actual-app/api for programmatic access to the SQLite directly.
- **Recommendation:** Build later
- **Notes:** Actual Budget users are exactly Trove's audience (local-first, privacy-minded). Reading their SQLite via M3 (copy-then-read, same pattern as iMessage chat.db) provides transactions with full categorization and budget context. Schema: `transactions`, `accounts`, `categories` tables are stable. Alternatively, the CSV export (via Actual's export feature) works as M1. The Node.js @actual-app/api package is not suitable for a Rust binary — use rusqlite to read the SQLite directly after copy. No FDA needed (user's own app data directory in ~/Library/Application Support/).

#### GoCardless Bank Account Data (EU/UK Open Banking) — _Open Banking (EU/UK)_

🟡 **Medium for EU/UK users specifically. GoCardless offers a free production tier and covers virtually all European banks through PSD2 mandates. Same standalone-distribution issue: developer-held keys. But the free tier (50 connections) is more workable for BYOK.** · M5 · API key (developer-held GoCardless secret — same distribution problem as Plaid for general users) · effort **L** · 🆕 new

- **Access:** GoCardless (formerly Nordigen) Account Information Services API. Registration at bankaccountdata.gocardless.com. Free tier: 50 monthly connections. REST API: https://bankaccountdata.gocardless.com/api/v2/accounts/{id}/transactions/. 2,500+ banks in EU/UK via PSD2 mandated open banking APIs. Secret key is developer-held.
- **Recommendation:** Spike first
- **Notes:** For EU/UK users, SimpleFIN has limited coverage (it's primarily a US service). GoCardless Bank Account Data is the natural equivalent — free tier, 2,500+ European banks, PSD2-compliant. The BYOK path (user registers their own GoCardless developer account — free, takes ~10 minutes) is more realistic here because EU/UK developers are already familiar with open banking APIs. Worth building as the EU/UK equivalent of SimpleFIN, with a clear 'register your own GoCardless account' onboarding flow. Confirm whether GoCardless's ToS permits BYOK embedded usage.

#### Grocery/Retail Loyalty Programs — _Retail Purchase Detail_

🟠 **Low — data request process is slow, format is inconsistent, and coverage depends on privacy law jurisdiction. No programmatic path. Kroger earned $527M selling shopper data but provides minimal structured export to consumers.** · M1 · none · effort **XL** · 🆕 new

- **Access:** No clean API or export path for consumers. GDPR/CCPA data requests (via state privacy laws in CA, TX, VA, OR, etc.) can force retailers to provide purchase data: kroger.com → Account → Data Privacy → Request My Data. Process takes weeks; format varies (often PDF or non-machine-readable). Albertsons/Safeway similar. No API.
- **Recommendation:** Icebox
- **Notes:** High user value (itemized grocery purchases would be the most detailed spending data available) but essentially blocked by lack of any API or structured export. The GDPR/CCPA data request path yields inconsistent data after weeks of waiting. A future regulatory shift (CFPB Section 1033 implementation) could change this. Icebox until a programmatic path exists. The email receipt approach (receipt.saveur.com, Fetch Rewards, etc.) is an indirect path if email corpus integration is built — Gmail/IMAP receipts from grocery pickup/delivery orders (Instacart, Amazon Fresh) contain itemized data. Note this in the email integration as a future enrichment opportunity.

#### Stripe Billing / Invoice Data — _SaaS Billing_

🟡 **Medium — only relevant for the subset of users who have a Stripe customer account (freelancers, developers, SaaS subscribers billed through Stripe). Not a mass-market integration.** · M1 · none (CSV export from Stripe dashboard for users with Stripe accounts) · effort **S** · 🆕 new

- **Access:** Stripe Dashboard → Billing → Invoices → Export → CSV. Also: Stripe API GET /v1/invoices with API key (requires Stripe account — only relevant for users who have a Stripe account as a customer of a platform, not general consumers). For personal use: Stripe sends email receipts for each charge that are capturable via email corpus.
- **Recommendation:** Icebox
- **Notes:** Most consumers encounter Stripe as the backend for other services' billing — they get email receipts but not a Stripe dashboard. The relevant population (users with direct Stripe accounts) is small. Email receipt parsing (planned as part of email corpus enrichment) is the right path for capturing SaaS subscription charges from any provider including Stripe-billed ones. Skip as a standalone integration; note in email enrichment roadmap.

#### Crypto Tax Aggregator Exports (Koinly, CoinTracker) — _Crypto Tax_

🟡 **Medium — useful for users who already use these services and want to seed Trove with their normalized crypto history. CSV export is clean. No ongoing sync path.** · M1 · none · effort **S** · 🆕 new

- **Access:** Koinly: Settings → Tax Reports → Export transactions as CSV (complete history, 10,000 transactions free). CoinTracker: Portfolio → Export → CSV. Both aggregate from exchanges and wallets via their own API connections. No official outbound API from either service.
- **Recommendation:** Build later
- **Notes:** Users who have gone through the pain of importing all their crypto history into Koinly/CoinTracker have a clean normalized CSV with cost basis, realized/unrealized gains, and exchange-normalized transaction types. This is better than re-importing exchange-by-exchange. A Koinly/CoinTracker CSV preset is a small addition to the existing importer. However, direct exchange API connections (Coinbase, Kraken) plus on-chain APIs (Etherscan, Blockstream) are the ongoing-sync path; tax aggregator exports are a one-time backfill. As of 2026, 1099-DA means exchanges must track and report basis — the need for third-party tax aggregators diminishes for straightforward cases.

#### FinanceKit iOS Companion (Apple Card/Cash live sync) — _Apple Financial Products (Live)_

🔴 **Blocked for the macOS app. Would require an iOS companion app, Apple entitlement approval, and a mechanism to sync data to the Mac vault (iCloud, local network, or USB). Technically feasible as a companion app but significant scope.** · M4 · Apple entitlement (requires approval) + user consent in Settings · effort **XL** · 📋 planned

- **Access:** FinanceKit API (iOS 17.4+, US only). Requires Apple-granted entitlement per bundle ID (entitlement request at developer.apple.com/financekit/). Swift API: FinanceStore.transactions(query:). Data: Apple Card, Apple Cash, Apple Savings balances + transactions. NOT available on macOS as of June 2026 — iOS/iPadOS only.
- **Recommendation:** Icebox
- **Notes:** This is the correct long-term path for live Apple Card/Cash/Savings data (Copilot, Monarch, and YNAB all use it on their iPhone apps). Blocked until either: (a) Apple brings FinanceKit to macOS, or (b) Trove ships an iOS companion app. The companion app path requires Apple entitlement approval (non-trivial process) plus the engineering overhead of an iOS app + vault sync. The CSV export path (M1 via Apple Card Statements on card.apple.com) is the correct near-term solution. Plan as a Phase 5 item after the iOS companion app is otherwise motivated.

### Financial, Spending & Purchases — cross-cutting notes

**The developer-key distribution problem is the single biggest structural constraint in this domain.** Plaid, Teller, MX, Finicity, Akoya, Yodlee, TrueLayer, SaltEdge, and SnapTrade all issue developer-held credentials that (a) ride on a developer quota, (b) bind Trove to those providers' ToS on behalf of all users, and (c) cannot safely ship in a distributed binary. SimpleFIN sidesteps this entirely (user-held credentials); GoCardless Bank Account Data is borderline (free BYOK plausible for EU users). All new aggregator evaluations should start with the question: 'who holds the credential?' before any technical assessment.

**File import is a permanent peer backend, not a fallback.** Apple Card, Apple Cash, Apple Savings, Venmo, Cash App, and all aggregator-excluded institutions are desktop-accessible ONLY via CSV/PDF export. The existing import engine (generic CSV + presets) should be extended continuously as new formats surface — every new preset is low effort and addresses real user need.

**Crypto is unusually clean.** On-chain data (Ethereum via Etherscan, Bitcoin via Blockstream) requires no credentials beyond a wallet address and one free API key. Exchange APIs (Coinbase, Kraken) use user-generated keys with no developer-app dependency. This domain bypasses the credential distribution problem entirely. IRS 1099-DA mandates (full gross proceeds 2025 tax year, cost basis 2026 tax year) mean exchanges now maintain complete, accurate records — API data quality will improve further.

**The dedup engine is load-bearing across all of financial.** Every new backend (YNAB, Lunch Money, investment exports) will create overlapping coverage with SimpleFIN. The existing cross-source fuzzy matcher (`finance/import.rs`) must be extended to handle non-bank transaction types (brokerage trades, crypto, P2P payments) where amount+date matching is less reliable (many trades at round numbers, P2P payments with duplicate amounts). Consider adding source-type context to the matching logic.

**OFX/QFX as a format is worth retaining as an import preset even though Direct Connect is dead.** Many smaller banks and credit unions still offer QFX/OFX file download from their web portals (distinct from Direct Connect protocol). Parsing QFX unambiguously is easier than column-sniffing CSV. The `finance/import.rs` follow-up item for QFX parsing should be prioritized.

**Investments data shapes the vault differently.** The current vault schema (`finance/transactions/`, `finance/balances/`) is bank-centric (posted date, amount, description). Investments need: holdings snapshots (date, symbol, quantity, cost basis, market value), trade records (buy/sell/dividend/split), and multi-currency support for international brokerages. Design the `finance/investments/` sub-schema before building any brokerage integration to avoid schema churn.

**EU/UK users need a different aggregation path.** SimpleFIN is US-centric. GoCardless Bank Account Data (free tier, 2,500+ European banks) is the natural equivalent. The bring-your-own-key path is more feasible here given EU developer familiarity with open banking. This is a meaningful gap for the 'built for anyone' principle.

---

## Social Media & Web Presence

Every major social platform now offers a GDPR-compliant "Download Your Data" export — this is the bedrock M1 path and works universally without API keys or platform approval. The open-protocol platforms (Bluesky/AT Protocol and Mastodon/ActivityPub) additionally offer clean no-auth or light-OAuth API pulls that are ideal for incremental sync. Closed platforms (Twitter/X, Instagram, Facebook, TikTok, Reddit) have tightened API access dramatically since 2023, making the archive export the primary practical path for all general users; live API sync is either prohibitively expensive (X), requires business-account registration (Instagram Graph), or pre-approval (Reddit). Feasibility is High across the board for M1 import; open-protocol platforms are High for live M5 pull; all other API paths are Medium-to-Low. Dating app exports are technically straightforward but privacy-sensitive by nature and should be opt-in and vault-isolated.

### At a glance

| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |
|---|---|---|---|---|---|---|
| Bluesky (AT Protocol) | Microblogging | M5 | OAuth (App Password for personal use, or OAuth 2 PKCE for proper app) | M | 🟢 High — fully open AT Protocol; no rate-limit or cost barrier for personal data; CAR export is unauthenticated; incremental sync is well-documented. | 🆕 new |
| Mastodon (Fediverse) | Microblogging | M1 | OAuth per user's instance server | M | 🟢 High — completely open API; no cost; no approval gate; well-documented. M1 archive gives full history; M5 API pull gives incremental updates. | 🆕 new |
| Twitter / X | Microblogging | M1 | Account login (no special permission) | S | 🟢 High — archive export works for any account; includes full tweet history, likes, DMs. API v2 is now pay-per-use (Feb 2026) making programmatic sync impractical for general users. | 🆕 new |
| Instagram | Photo/Video Sharing | M1 | Account login (Meta Accounts Center) | S | 🟢 High for M1 (works for all account types). Medium for M5 API (Business/Creator accounts only; requires Facebook Page linkage and App Review for production use). | 🆕 new |
| Facebook | Social Network | M1 | Account login | S | 🟢 High — comprehensive export; JSON format well-structured for parsing. | 🆕 new |
| Reddit | Forum / Communities | M1 | Account login; API requires OAuth + app pre-approval | S | 🟢 High for M1. Medium for M5 API (pre-approval required, 100 QPM limit — adequate for personal use but approval friction is a barrier). | 🆕 new |
| TikTok | Short-Form Video | M1 | Account login | S | 🟢 High for M1. The Data Portability API is an M5 option but requires developer registration. | 🆕 new |
| LinkedIn | Professional Network | M1 | Account login | S | 🟢 High — LinkedIn is universally used by professionals; export is comprehensive. | 🆕 new |
| Substack (as writer) | Newsletter / Publishing | M1 | Account login (writer/publisher account) | S | 🟢 High for M1 — posts export is straightforward HTML-in-ZIP. Stats CSV also now available. No official API so M5 is unsupported officially. | 🆕 new |
| Discord | Messaging / Communities | M1 | Account login | M | 🟢 High for M1 official data request — covers all personal messages. DiscordChatExporter is a useful M6 supplement but requires user token (ToS gray area). | 🆕 new |
| Snapchat | Ephemeral Messaging / Photo | M1 | Account login | S | 🟢 High — export is comprehensive and JSON-parseable. | 🆕 new |
| Tumblr | Blogging | M1 | Account login | S | 🟢 High — fast export; well-structured JSON; API also available with free OAuth app registration. | 🆕 new |
| Pinterest | Visual Bookmarking | M1 | Account login; API needs OAuth + developer account | S | 🟡 Medium — official export covers metadata but not images (URLs only). API access is available but requires developer registration. | 🆕 new |
| Threads (Meta) | Microblogging | M1 | Account login (same as Instagram — via Meta Accounts Center) | S | 🟢 High — bundled with Instagram export; no extra steps. Threads-specific API is not yet publicly available. | 🆕 new |
| YouTube (as creator) | Video / Creator Analytics | M1 | Google account login; Analytics API requires OAuth | M | 🟢 High for M1 Takeout — watch history and liked videos are the key assets for personal use. Medium for creator analytics API (OAuth + project setup required). | 📋 planned |
| Tinder | Dating App | M1 | Account login | S | 🟢 High technically — JSON export is well-structured. Privacy-sensitive by nature: match history, message content, swipe behavior. Should be opt-in with clear privacy disclosure. | 🆕 new |
| Hinge | Dating App | M1 | Account login | S | 🟢 High technically. Privacy-sensitive — opt-in only. | 🆕 new |
| Bumble | Dating App | M1 | Account login | S | 🟡 Medium — 30-day processing window is slow; format less documented than Tinder/Hinge. Privacy-sensitive. | 🆕 new |
| Strava (social layer) | Sports / Social | M1 | Account login; API requires OAuth app registration | M | 🟢 High for M1 bulk export (activities CSV + FIT files). Medium for M5 API (OAuth registration required but straightforward). | 🆕 new |
| BeReal | Photo Sharing | M1 | Account login; request via in-app support chat | M | 🟡 Medium — no self-serve export button (requires support chat); raw photo format needs conversion; platform has declined in popularity (40M MAU in 2026 vs peak 73M in 2022). | 🆕 new |
| Twitch | Live Streaming | M1 | Account login; API requires OAuth | M | 🟡 Medium — official data download exists but format/completeness is less well-documented than Meta/Google platforms. Primarily relevant for streamers, not viewers. | 🆕 new |
| Google Analytics / GA4 (personal site/blog) | Website Analytics | M5 | Google OAuth (Analytics.readonly scope); BigQuery needs GCP account | M | 🟡 Medium — requires user to have GA4 installed on their site AND a Google account with Analytics access. Niche use case (personal site owners). | 🆕 new |
| Plausible Analytics (personal site/blog) | Website Analytics | M5 | API key (no OAuth; bearer token in header) | S | 🟢 High — simpler than GA4; API key based (no OAuth); self-hosted users own their ClickHouse DB directly. Privacy-respecting by design (no PII). | 🆕 new |
| OkCupid | Dating App | M1 | Account login + support request | M | 🟠 Low — no self-serve export; requires support ticket; format not well-documented; FTC privacy issues add reputational concern. Match Group security breach (2026) exposed OkCupid data. | 🆕 new |

### Detail

#### Bluesky (AT Protocol) — _Microblogging_

🟢 **High — fully open AT Protocol; no rate-limit or cost barrier for personal data; CAR export is unauthenticated; incremental sync is well-documented.** · M5 · OAuth (App Password for personal use, or OAuth 2 PKCE for proper app) · effort **M** · 🆕 new

- **Access:** No-auth repo export: GET https://<pds-host>/xrpc/com.atproto.sync.getRepo?did=<did> returns a CAR file. Authenticated incremental pull via OAuth 2 (since Sep 2024) or legacy App Password: GET /xrpc/app.bsky.feed.getAuthorFeed, /xrpc/app.bsky.feed.getLikes, /xrpc.app.bsky.graph.getFollowers, etc. No API key required, no paid tier, no app-review gate.
- **Recommendation:** Build now — best-in-class open social API; the CAR repo export gives complete post/like/follow history in one shot; incremental M5 pull handles new posts.
- **Notes:** CAR file is a CBOR-encoded Content Addressable aRchive (DAG-CBOR records inside). Parse with the `iroh-car` or `atrium` Rust crates. Public posts only — DMs (via Constellation, Bluesky's DM layer) are not in the public repo. Repo export has no auth requirement, so no credentials needed for a full historical pull of the user's own DID.

#### Mastodon (Fediverse) — _Microblogging_

🟢 **High — completely open API; no cost; no approval gate; well-documented. M1 archive gives full history; M5 API pull gives incremental updates.** · M1 · OAuth per user's instance server · effort **M** · 🆕 new

- **Access:** Built-in archive export: Settings > Import and Export > Request archive — ZIP containing actor.json (ActivityPub actor), outbox.json (all posts in ActivityStreams 2.0 JSON-LD), bookmarks.json, likes.json, media_attachments/. Available every 7 days. Also: Mastodon REST API — GET /api/v1/statuses, /api/v1/favourites, /api/v1/accounts/verify_credentials with OAuth 2 user token (scopes: read:statuses, read:favourites, read:accounts). Instance URL is user-provided; API documented at docs.joinmastodon.org.
- **Recommendation:** Build now — open protocol with strong data portability. Implement both M1 archive import and M5 incremental pull. Instance URL is required config.
- **Notes:** The archive export (M1) covers all posts including boosts and media. The API (M5) can poll incrementally using max_id cursor. Mastodon v4.5.6 is current as of 2026. The 7-day export cadence means M1 alone is not real-time; pair with M5 for freshness. Does not export followers list in the archive — only following/blocks/mutes CSVs. ActivityPub JSON-LD is parseable with serde_json.

#### Twitter / X — _Microblogging_

🟢 **High — archive export works for any account; includes full tweet history, likes, DMs. API v2 is now pay-per-use (Feb 2026) making programmatic sync impractical for general users.** · M1 · Account login (no special permission) · effort **S** · 🆕 new

- **Access:** Account archive export: Settings > Your Account > Download an archive of your data — ZIP containing data/ folder with per-section JS files (tweet.js, like.js, direct-messages.js, follower.js, following.js, account.js, ad-engagements.js, etc.) plus HTML viewer. Files are JS with a window.YTD.tweets.part0 = [...] wrapper; strip the assignment prefix to get valid JSON. Link expires after 7 days.
- **Recommendation:** Build now (M1 only) — API costs are prohibitive ($0.005/read, capped at 2M reads/month on pay-per-use) for a general tool. Archive export covers complete history. Skip API integration.
- **Notes:** Export ZIP includes: tweets (full text, media, URLs), DMs (full thread history), likes, followers/following counts, lists, moments, ad data. Media files (images/videos) are included. The JS-wrapper format (not pure JSON) requires stripping `window.YTD.<name>.part0 = ` prefix before JSON parsing. Tweet body is in the `full_text` field (not truncated). Re-export required for updates since archives are one-shot snapshots; suggest prompting user to re-import periodically. API M5 path: technically possible at $0.001/read for 'owned reads' (own posts, bookmarks, followers) as of April 2026, but this still requires paying — not suitable as a default for general users.

#### Instagram — _Photo/Video Sharing_

🟢 **High for M1 (works for all account types). Medium for M5 API (Business/Creator accounts only; requires Facebook Page linkage and App Review for production use).** · M1 · Account login (Meta Accounts Center) · effort **S** · 🆕 new

- **Access:** Download Your Information: Accounts Center > Your information and permissions > Export your information > Create export > Export to device. Select JSON format (not HTML). Covers posts, stories, reels, DMs, followers/following, comments, likes, profile info, ad interactions. ZIP delivered via email within ~1 hour. Also: Instagram Graph API (M5) at graph.facebook.com/v21.0/{ig-user-id}/media — requires Business or Creator account linked to a Facebook Page; Personal accounts CANNOT use the API (Basic Display API EOL Dec 4, 2024).
- **Recommendation:** Build now (M1) — works universally. Skip M5 API for now (Business/Creator only, not general-user accessible).
- **Notes:** JSON export structure: media/ folder for photos/videos, content/posts_1.json, messages/inbox/<thread>/ for DMs, connections/followers_and_following/, ads_information/. DM export includes full message history. Media quality selection available (low/medium/high) at export time. Threads data is bundled in the same Accounts Center export — the same ZIP contains both Instagram and Threads data, so a single import path handles both. Reminder: no API access for personal accounts since Dec 2024.

#### Facebook — _Social Network_

🟢 **High — comprehensive export; JSON format well-structured for parsing.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Download Your Information: Settings > Your Facebook Information > Download Your Information (also at facebook.com/dyi). Select JSON format. Selectable categories: posts, photos/videos, messages, friends/followers, profile info, comments, reactions, search history, marketplace, events, groups, ads, location, security. Date range filter available. ZIP delivered via notification/email, usually within a few hours.
- **Recommendation:** Build now — large user base; straightforward M1 import; covers posts, messages (Messenger), photos.
- **Notes:** JSON export structure: posts/your_posts_1.json, messages/inbox/<thread>/ (full Messenger history), photos_and_videos/, friends/friends.json. Messenger export includes full conversation history with timestamps. Media files included in ZIP. 'All Time' date range available. Facebook Graph API is not a practical path for personal data — it requires app review and is gated for consumer/personal use. M1 is the only viable path for general users.

#### Reddit — _Forum / Communities_

🟢 **High for M1. Medium for M5 API (pre-approval required, 100 QPM limit — adequate for personal use but approval friction is a barrier).** · M1 · Account login; API requires OAuth + app pre-approval · effort **S** · 🆕 new

- **Access:** Data request: reddit.com/settings/data-request (or Settings > Privacy & Safety > Request a copy of your data). Select GDPR option. Returns CSV files: comments.csv (full comment history with text, subreddit, score, timestamp), posts.csv (submissions), chat_history.csv (DM chats). Takes up to 30 days; typically faster. Also: Reddit API v2 with OAuth — 100 QPM for authenticated calls; requires app registration and pre-approval (as of 2025 crackdown, all apps including personal scripts need approval).
- **Recommendation:** Build now (M1) — straightforward CSV import. Spike M5 API later if incremental sync proves valuable.
- **Notes:** CSV format is simple: comments.csv has id, permalink, date, ip, subreddit, gildings, link, parent, body, score. Data request can take up to 30 days (GDPR timeline). Reddit API v2: personal-use script apps still available but require Reddit approval since 2025 crackdown. Pushshift historical archives still accessible at files.pushshift.io for bulk historical analysis but is not a personal-data API. Saved posts are NOT included in the standard data export — a known gap.

#### TikTok — _Short-Form Video_

🟢 **High for M1. The Data Portability API is an M5 option but requires developer registration.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Request Your Data: Profile > hamburger > Settings and privacy > Account > Download your data. Select JSON format (not TXT — TXT is far less complete). Categories include: video browsing history (10k-50k+ entries for active users), liked videos, comments, DMs, browsing history, ad interests, app settings. Typically ready in 1-4 days; up to 30 days per policy. TikTok also has a Data Portability API (developers.tiktok.com/doc/data-portability-api-download) for programmatic access.
- **Recommendation:** Build now (M1) — JSON export is comprehensive and well-structured. Spike M5 Data Portability API later for incremental sync.
- **Notes:** JSON export organized into Activity/ (browsing history, liked videos, comments, shares), Ads and Data/, App Settings/, Direct Messages/. Browsing history is particularly rich: every video watched with timestamps. Download link valid for only a few days after notification — Trove should import promptly. US legal status of TikTok has been uncertain; as of June 2026 the app is operational but this could change — M1 archive import is resilient to platform shutdown.

#### LinkedIn — _Professional Network_

🟢 **High — LinkedIn is universally used by professionals; export is comprehensive.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Settings & Privacy > Data privacy > Get a copy of your data > select data types (Connections, Messages, Posts, Comments, Reactions, Invitations, Profile, etc.) > Request archive. Connections CSV ready in ~10-24 min; full archive up to 72 hours. CSV format for most data; some as JSON. Also available: Settings > Data privacy > Download your data for a scoped fast export of just Connections.
- **Recommendation:** Build now — broad user relevance; connections CSV (name, company, position, connected date) and messages are the high-value payloads.
- **Notes:** Connections.csv includes: First Name, Last Name, Email Address (often missing — user-controlled), Company, Position, Connected On. Full archive adds: messages (CSV), articles you wrote (HTML), posts/comments, profile info, search history, ads data. Email addresses are frequently absent due to LinkedIn privacy defaults — do not assume they are present. LinkedIn API (REST) is available but requires partner approval for data access beyond basic profile — not practical for a general tool.

#### Substack (as writer) — _Newsletter / Publishing_

🟢 **High for M1 — posts export is straightforward HTML-in-ZIP. Stats CSV also now available. No official API so M5 is unsupported officially.** · M1 · Account login (writer/publisher account) · effort **S** · 🆕 new

- **Access:** Writer export: Publication Dashboard > Settings (bottom-left) > Import/Export > Export. Delivers posts as HTML files in a ZIP. Subscriber list export: Subscriber Dashboard > Export CSV (name, email, subscription status, subscription date, revenue). Stats CSV export added March 2026: Dashboard > Analytics > Export as CSV. No official public API; unofficial API endpoints exist (e.g., https://<pub>.substack.com/api/v1/posts?offset=0&limit=50) and are used by community tools.
- **Recommendation:** Build now (M1) — relevant for any writer using Substack; HTML post export plus stats CSV covers the key data.
- **Notes:** Post export ZIP contains one HTML file per post. Subscriber export CSV includes email addresses — treat as sensitive PII in the vault. Paid subscriber revenue data is included. No read/engagement per-post breakdown in the native export (only aggregate stats). The unofficial API at https://<pub>.substack.com/api/v1/posts returns JSON with full post metadata and content — usable as an M5 supplement for a user's own publication, but unofficial and subject to change. Also note: as a reader/subscriber, there is no Substack data export for newsletters you subscribe to.

#### Discord — _Messaging / Communities_

🟢 **High for M1 official data request — covers all personal messages. DiscordChatExporter is a useful M6 supplement but requires user token (ToS gray area).** · M1 · Account login · effort **M** · 🆕 new

- **Access:** Data request: User Settings > Privacy & Safety > Data Request > Request all of my data. Takes 3-30 days. ZIP contains JSON files: messages/ folder (one subfolder per channel/DM/group with channel.json metadata and messages.json), account/ (profile info), servers/, activity/, payments/. Also: DiscordChatExporter (open-source .NET tool, github.com/Tyrrrz/DiscordChatExporter) can export any channel you have access to via user token — HTML/CSV/JSON/TXT formats.
- **Recommendation:** Build now (M1) — Discord is widely used; messages folder contains full DM and server message history. 3-30 day wait is a UX friction point to document.
- **Notes:** Messages folder structure: messages/c<channel_id>/ with messages.json (array of {id, timestamp, contents, attachments}) and channel.json (channel name/type). This is a one-time snapshot — no incremental export supported. The data request covers only messages YOU SENT, not the full conversation thread (other participants' messages not included in your export). Media attachments are referenced by URL only, not downloaded. DiscordChatExporter alternative can fetch full thread context but uses self-bot tokens which Discord's ToS prohibits for automation.

#### Snapchat — _Ephemeral Messaging / Photo_

🟢 **High — export is comprehensive and JSON-parseable.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** My Data export: accounts.snapchat.com > My Data (or Settings in-app > My Data). Select data categories: Login History, Account Information, Snap History, Saved Chat History, Memories, Purchase History, Friends, Location, Search History, Bitmoji. Format: ZIP with HTML index + JSON files. Ready within 24-48 hours (up to 7 days for large exports).
- **Recommendation:** Build later — useful for users who use Snapchat heavily; Memories (saved snaps) and chat history are the primary data assets. Lower priority than text-heavy platforms.
- **Notes:** Memories are the most valuable asset — saved photos/videos the user chose to keep. Snap History shows metadata of snaps sent/received (not content of ephemeral snaps, which are deleted). Saved Chat History covers text messages saved in chats. Open-source tools (bereal-gdpr-photo-toolkit-style) exist for processing the ZIP. Media files referenced by URL in the JSON — not included in ZIP directly. Memories are stored as separate media files in the export.

#### Tumblr — _Blogging_

🟢 **High — fast export; well-structured JSON; API also available with free OAuth app registration.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Export: go directly to tumblr.com/settings/blog/<YOURBLOGNAME>/export (or Settings > Account > Export Data). ZIP generated in ~38 seconds median (very fast). Contains: posts in JSON (full content, tags, timestamps, notes), media files (photos/videos), plus HTML viewer. Also: Tumblr API v2 with OAuth 2 — GET /v2/blog/{identifier}/posts (own blog), GET /v2/user/likes, GET /v2/user/following. Registered app required (free, no approval gate).
- **Recommendation:** Build later — Tumblr has a dedicated but smaller user base; JSON export is fast and complete. Could pair M1 with M5 API for incremental sync.
- **Notes:** Export JSON is ActivityPub-compatible. API v2 is live and supports OAuth 2 (since 2021). Rate limits are undocumented but generous for personal use. Multi-blog accounts each need a separate export. The export covers posts on YOUR blog; liked/reblogged content from others is tracked by URL reference only.

#### Pinterest — _Visual Bookmarking_

🟡 **Medium — official export covers metadata but not images (URLs only). API access is available but requires developer registration.** · M1 · Account login; API needs OAuth + developer account · effort **S** · 🆕 new

- **Access:** Data download: Settings > Privacy and Data > Request your data. Email with ZIP link within 48 hours. Contains: account info, board metadata, pin URLs (not images), follower/following lists. No pixel-perfect image archive — pin URLs point to Pinterest CDN. Pinterest API v5 (developers.pinterest.com) provides OAuth access to boards, pins, and analytics — free with developer account registration.
- **Recommendation:** Build later — niche use case; metadata export (board names, pin URLs, notes) is the useful payload. Images require separate download from URLs.
- **Notes:** The official export is notably thin compared to other platforms — boards and pins are represented as metadata with URLs, not downloaded content. Pin descriptions, board names, and link targets are the key data. Pinterest API v5 provides GET /boards, /pins endpoints with OAuth — could be used for incremental sync but requires developer account. Third-party scrapers (Pinback bookmarklet) can export pin links to HTML bookmarks as an alternative.

#### Threads (Meta) — _Microblogging_

🟢 **High — bundled with Instagram export; no extra steps. Threads-specific API is not yet publicly available.** · M1 · Account login (same as Instagram — via Meta Accounts Center) · effort **S** · 🆕 new

- **Access:** Bundled into the same Meta Accounts Center export as Instagram: Accounts Center > Your information and permissions > Export your information. Same flow, same ZIP — Threads data appears alongside Instagram data. JSON or HTML format. Date range filter available. Threads data includes: posts, replies, likes, followers/following.
- **Recommendation:** Build as part of Instagram importer — the same ZIP contains both; parse the Threads subfolder alongside Instagram data at no additional effort.
- **Notes:** Threads export is inside the same ZIP as Instagram — no separate export request needed. Posts and replies are in threads_and_replies.json. No public Threads API as of June 2026 (Meta has not opened one beyond the bundled export). ActivityPub federation from Threads is in progress but not yet a stable data-access path for Trove.

#### YouTube (as creator) — _Video / Creator Analytics_

🟢 **High for M1 Takeout — watch history and liked videos are the key assets for personal use. Medium for creator analytics API (OAuth + project setup required).** · M1 · Google account login; Analytics API requires OAuth · effort **M** · 📋 planned

- **Access:** Google Takeout (takeout.google.com): select YouTube — exports watch history (JSON or HTML), search history (JSON), liked videos (JSON playlist), subscriptions (CSV), comments (JSON), channel uploads metadata. Also: YouTube Studio > Analytics > Advanced Mode > Export > CSV (up to 500 rows per view) or YouTube Analytics API (developers.google.com/youtube/analytics) for automated reporting. YouTube Data API v3 for channel/video metadata.
- **Recommendation:** Build now (M1 Takeout) — watch history overlaps with the planned YouTube Takeout integration; make sure to include creator analytics CSV for users with channels. The existing planned YouTube Takeout item should include creator-side data.
- **Notes:** Watch history JSON includes: videoId, title, titleUrl, subtitles (channel name/URL), time (ISO 8601). Liked videos JSON: list of video objects in playlist format. Takeout also includes: comments you posted (JSON), subscriptions (CSV with channel title/URL), chat messages from live streams. For creators: Studio analytics CSV export is limited to 500 rows; YouTube Reporting API (bulk exports via scheduled jobs) supports full data dumps. Note: YouTube Takeout history is already in the planned list — ensure creator analytics is included.

#### Tinder — _Dating App_

🟢 **High technically — JSON export is well-structured. Privacy-sensitive by nature: match history, message content, swipe behavior. Should be opt-in with clear privacy disclosure.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Data export: account.gotinder.com/data (or Settings > Get My Data in-app). Returns a data.json file (sometimes ZIP). Takes 1-3 days. Contents: account info, preferences, match history with message threads, swipe statistics (right/left/super by day), photos, ad data, purchased products.
- **Recommendation:** Build later — implement as an opt-in privacy-sensitive import; vault subfolder with user-acknowledged sensitivity flag. Useful for users who want complete personal data.
- **Notes:** tinder.json structure includes: Usage (daily swipe counts by type), Messages (per-match thread with timestamps), Photos (CDN URLs), Purchases. Match names are not included in the export — matches appear by match ID only (privacy design by Tinder). Message content is included in full. Export link expires 48 hours after generation. SwipeStats.io is a well-known third-party analyzer of this data, confirming the format is stable and parseable.

#### Hinge — _Dating App_

🟢 **High technically. Privacy-sensitive — opt-in only.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Data export: Settings > Download My Data (in-app). Returns a ZIP with JSON files. Takes 1-3 days. Contents: matches.json (match history, messages sent/received), events.json (swipe/like/skip activity), user.json (profile), media/ (photos).
- **Recommendation:** Build later — same opt-in privacy-sensitive treatment as Tinder. Implement alongside Tinder as a Dating Apps import category.
- **Notes:** matches.json includes full message thread history with timestamps. Unlike Tinder, Hinge includes match names/identifiers. events.json covers like/skip/block activity with timestamps. Match Group (Hinge, Tinder, OkCupid, Match) suffered a data breach exposure in 2026 (FTC action against OkCupid for sharing photos) — relevant context for privacy framing in the UI.

#### Bumble — _Dating App_

🟡 **Medium — 30-day processing window is slow; format less documented than Tinder/Hinge. Privacy-sensitive.** · M1 · Account login · effort **S** · 🆕 new

- **Access:** Data request: Settings > Contact & FAQ > Request My Data. Up to 30 days processing time. Returns account data, match history, and messages. Format is ZIP with JSON.
- **Recommendation:** Icebox — slower and less documented than Tinder/Hinge; implement after the core dating-app import pattern is proven.
- **Notes:** The 30-day wait (vs 1-3 days for Tinder/Hinge) is a significant UX friction point. Data content is expected to be similar (matches, messages) but the exact JSON schema is less publicly documented. Consider grouping all dating apps under one import category with shared parsing logic where possible.

#### Strava (social layer) — _Sports / Social_

🟢 **High for M1 bulk export (activities CSV + FIT files). Medium for M5 API (OAuth registration required but straightforward).** · M1 · Account login; API requires OAuth app registration · effort **M** · 🆕 new

- **Access:** Bulk export: strava.com/athlete/delete_your_account (same page as account download, not deleted): Settings > My Account > Download or Delete Your Account > Get Started > Request Download. ZIP contains activities/ (FIT/GPX files for each activity), activities.csv (metadata for all activities), profile data, routes, photos. Individual activity: activity page > ... > Export GPX or Export FIT. Also: Strava API v3 with OAuth — GET /athlete/activities, /activities/{id}, /athlete/stats — 200 req/15min, 2000/day.
- **Recommendation:** Spike first — Strava is primarily a fitness/GPS tracker (overlaps with health domain) but has a strong social graph (segments, kudos, clubs). Coordinate with health domain agent; consider whether Strava social data (kudos, followers, segment leaderboard rank) belongs here or in health.
- **Notes:** Strava bulk export ZIP includes: activities.csv (activity name, date, type, distance, moving time, elapsed time, elevation, gear, private flag), individual FIT/GPX files for GPS tracks, routes.csv, profile.json. The social layer (kudos given/received, followers/following, club memberships) is accessible via API (GET /athlete/friends, /clubs/{id}/members) but NOT included in the bulk export. FIT file parsing needs a Rust FIT library (fit-rs or fit-file crate). API as of June 2026: 200 req/15min / 2000/day default; 'Extended Access Tier' available with higher limits.

#### BeReal — _Photo Sharing_

🟡 **Medium — no self-serve export button (requires support chat); raw photo format needs conversion; platform has declined in popularity (40M MAU in 2026 vs peak 73M in 2022).** · M1 · Account login; request via in-app support chat · effort **M** · 🆕 new

- **Access:** GDPR data request via in-app chat support (no self-serve menu). Returns ZIP within 48 hours containing: all BeReal photos (front + back camera as separate files, raw format needing conversion to standard JPG), account metadata (username, phone, registration date, privacy settings history), login history. Open-source processing tools: bereal-data-transform (GitHub), bereal-gdpr-photo-toolkit (GitHub), BeReal GDPR Explorer (browser-based).
- **Recommendation:** Build later — niche platform; friction in the export process; but the dual-camera daily photo format is unique and personally meaningful. Worth implementing if user base justifies it.
- **Notes:** BeReal dual-camera images are stored in a proprietary raw format in the GDPR export that requires decoding (not standard JPEG). The open-source bereal-gdpr-photo-toolkit handles this conversion. Export contains a complete daily photo archive — every BeReal taken, with timestamps. Front and back camera stored as separate image files. No API available.

#### Twitch — _Live Streaming_

🟡 **Medium — official data download exists but format/completeness is less well-documented than Meta/Google platforms. Primarily relevant for streamers, not viewers.** · M1 · Account login; API requires OAuth · effort **M** · 🆕 new

- **Access:** Personal data download: Settings > Security and Privacy > Download Your Data (also at twitch.tv/privacy/controls). Returns a data package (format/timeline not fully documented in public help articles; typically takes up to 30 days). Twitch API v5/Helix: GET /channels, /clips, /videos, /subscriptions — OAuth required; access token via Authorization Code flow.
- **Recommendation:** Build later — lower priority; most relevant for people who stream or chat heavily on Twitch. API access (M5) is a viable alternative for creator analytics.
- **Notes:** Twitch does not publish a detailed breakdown of what the data download contains. API Helix endpoints provide channel analytics (subscribers, bit cheers, clip views) for streamers. For viewers: watch history is not exposed via API or reliably via data download. Chat export tools (exportcomments.com/export-twitch-chat) can dump VOD chat history to CSV/JSON. VOD metadata (not video files) is accessible via API.

#### Google Analytics / GA4 (personal site/blog) — _Website Analytics_

🟡 **Medium — requires user to have GA4 installed on their site AND a Google account with Analytics access. Niche use case (personal site owners).** · M5 · Google OAuth (Analytics.readonly scope); BigQuery needs GCP account · effort **M** · 🆕 new

- **Access:** UI export: GA4 Studio > Reports > Explore > Export CSV (limited to 500 rows for standard reports, unlimited via Advanced export). Google Analytics Data API v1 (developers.google.com/analytics/devguides/reporting/data/v1): POST /v1beta/properties/{propertyId}:runReport — OAuth 2, free API, no per-call cost. Also: GA4 BigQuery Export (streams all raw event-level data to BigQuery — requires GCP account, free to enable on standard properties).
- **Recommendation:** Build later — relevant for bloggers/creators with personal sites using GA4. M5 API pull is clean but requires OAuth setup and property ID configuration.
- **Notes:** GA4 Data API is free; OAuth scopes: analytics.readonly. Property ID is user-configured. Metrics available: sessions, pageviews, users, bounce rate, engagement time, top pages, traffic sources, countries. Raw event-level data requires BigQuery export (GCP setup). Important: GA4 collects anonymous aggregate data, not personally identifiable user journeys — so this is the site OWNER's analytics, not user-level data.

#### Plausible Analytics (personal site/blog) — _Website Analytics_

🟢 **High — simpler than GA4; API key based (no OAuth); self-hosted users own their ClickHouse DB directly. Privacy-respecting by design (no PII).** · M5 · API key (no OAuth; bearer token in header) · effort **S** · 🆕 new

- **Access:** Stats API: POST /api/v1/stats (cloud) or same endpoint on self-hosted instance. Requires API key (generated in account settings). Returns JSON. Also: CSV export via dashboard (filtered stats). Self-hosted: ClickHouse database directly accessible on own server. Community Edition (CE) v2.2 (March 2026): fully open-source AGPL-3.0, same API as cloud.
- **Recommendation:** Build now if Trove targets indie makers/bloggers — simplest analytics API integration (single API key, clean JSON, no auth dance). Pairs well with GA4 as an alternative.
- **Notes:** Plausible API endpoint: GET/POST https://plausible.io/api/v1/stats/aggregate (cloud) or http://localhost:8000/api/v1/stats/aggregate (self-hosted). API key goes in Authorization: Bearer <key> header. Metrics: visitors, pageviews, bounce_rate, visit_duration, events. Filters by page, source, country, device, browser. Self-hosted users can also query ClickHouse directly for raw event-level data (zero vendor lock-in). No PII — site visitor data is aggregate/anonymous, so this is pure analytics for the site owner.

#### OkCupid — _Dating App_

🟠 **Low — no self-serve export; requires support ticket; format not well-documented; FTC privacy issues add reputational concern. Match Group security breach (2026) exposed OkCupid data.** · M1 · Account login + support request · effort **M** · 🆕 new

- **Access:** Data export via support request (no self-serve menu as of 2026). Contact support to request personal data under GDPR/CCPA. Returns JSON or CSV. OkCupid had an FTC enforcement action (March 2026) for sharing user photos and data with third parties without consent — the FTC settlement prohibits future misuse.
- **Recommendation:** Icebox — no self-serve export, security track record issues, low incremental value vs. Tinder/Hinge which both have self-serve exports.
- **Notes:** OkCupid is part of Match Group (same as Tinder). The March 2026 FTC action found OkCupid shared nearly 3 million user photos with Clarifai for AI training without user consent. Settlement permanently prohibits this. As a Trove data source, it is importable via support request but the lack of self-serve export and the privacy concerns mean it should be deprioritized.

### Social Media & Web Presence — cross-cutting notes

1. M1 (archive export) is the universal baseline — every platform covered here offers a GDPR-mandated export, and the format is consistently JSON (preferred) or CSV. Build a shared import pipeline: accept a ZIP or folder drop, detect the platform by directory structure/sentinel files (tweet.js, tinder.json, messages/inbox/, etc.), and route to the appropriate parser. The Twitter/X JS-wrapper quirk (strip window.YTD.* = prefix) and the BeReal raw photo format are the only notable non-standard cases.

2. Open-protocol platforms (Bluesky AT Protocol via com.atproto.sync.getRepo, Mastodon via REST API + archive ZIP) are the only sources supporting both full-history import AND incremental live sync without cost or approval gates. Prioritize these for M5 alongside M1 — they are the model to follow.

3. API access is mostly blocked or expensive for closed platforms: X/Twitter API is now pay-per-use with no free tier for new developers; Instagram Graph API requires Business/Creator account + Facebook Page linkage (personal accounts locked out since Dec 2024); Reddit API requires pre-approval since 2025. For all three, M1 archive export is the only practical general-user path.

4. Dating apps (Tinder, Hinge, Bumble) require special handling: implement as an opt-in category with explicit user acknowledgment of sensitivity, store under per-source folders like any social source (`~/Trove/social/<source>/`, e.g. `social/tinder/`) with the privacy-sensitive needs-flag set, and document that contents include match history and message text. The same JSON import pattern works across Tinder/Hinge.

5. Threads shares an export with Instagram — parse both from the same Accounts Center ZIP. This is a free win — one import handles two platforms.

6. Website analytics sources (Plausible, GA4) are niche but represent a distinct type of social web presence data (site traffic vs. personal posts). Plausible is the easier integration (API key, no OAuth) and should be done first if this category is prioritized.

7. A single shared 'social archive importer' UI pattern — drag a ZIP, auto-detect platform, show summary, confirm — would serve Twitter/X, Instagram/Threads, Facebook, TikTok, LinkedIn, Reddit, Discord, Snapchat, Tumblr, and Substack with largely the same UX flow. Build the detection/routing layer once; each platform gets a parser module.

---

