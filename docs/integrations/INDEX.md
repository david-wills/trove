# Integration Build Queue — `INDEX.md`

The Phase 4 build loop reads this file **top-down**: it picks the
lowest-numbered 📋 queued row, builds it on its own short-lived worktree,
lands it on `integration-staging`, and moves on. **Reordering the queue is
editing this file** — order numbers are the contract, not the row position
in any other doc. One row per provider (combined by provider). Every
provider in the catalog appears here, including 🚫 unavailable ones, so the
app can answer "is X integrated, and if not why not?" Each row links to its
brief, the build spec and living status record.

Generated in the Phase 2 catalog pass from `docs/integrations-research.md`
via the per-provider briefs. Doctrine: `docs/integration-pipeline.md`.
Loop contract: `docs/collector-loop.md`. Taxonomy: `docs/integrations/README.md`.

## Status legend

🚫 unavailable · 📋 queued · 🚧 building · 🧪 built (fixture-tested, not
validated) · ✅ validated (real-data confirmed — only David promotes) ·
📦 built, not in this build (pruned 2026-09-14 per `docs/roadmap.md`; the
module is restorable from commit `33bda15` — see below).
Build vs. validate are separate axes; the brief's validation matrix tracks
it per capability slice.

**Needs-flags:** 🔒 privacy-sensitive (ships opt-in with explicit
acknowledgement; password-manager imports hard-strip secrets at parse
time) · Needs-sample (folklore export format — parser built last, awaiting
a real file) · Needs-login (real-data validation needs an account; the
build proceeds from documented shapes) · Needs-David (a decision is gated
on David — see the brief).

**Sort:** queued in build order (P0→P1→P2; within a band: time-sensitive
first, then the first entry seeding a new domain, then smaller effort
first), then 🧪 built (alphabetical), then 🚫 unavailable (alphabetical).
Order numbers apply to queued rows only; built and unavailable show `—`.
Derived read-time features (person graph, entity resolution) are not
collectors and sit in the **Deferred** section below the queue — the loop
skips them; they're revisited post-wave per the pipeline doctrine.

**Restoring a 📦 row:** its brief names the module file(s) under
`crates/trove-core/src/`. Run `git checkout 33bda15 -- <those files>`, add
`pub mod <name>;` to `lib.rs` and the `&crate::<name>::DEF` (and
`::CONNECTION`, if any) line to `integrations.rs`, then
`TROVE_REGEN=1 cargo test -p trove-core --test schedule_doc`. The app's
Integrations → Catalog section shows the same list with these details.

| # | Provider | Name | Status | Domain | Priority | Effort | Needs | Summary |
|---|---|---|---|---|---|---|---|---|
| 1 | [`lastfm`](./lastfm.md) | Last.fm | 📦 | `media/` | P0 | S | Needs-login | The original music scrobbling service: any player that scrobbles (Spotify plugin, the Last.fm apps, Navidrome, Tidal's built-in scrobbler, Trove's own… |
| 2 | [`listenbrainz`](./listenbrainz.md) | ListenBrainz | 📦 | `media/` | P0 | S | — | The open-source scrobbling service from MetaBrainz (the MusicBrainz people): the community-governed alternative to Last.fm, with MusicBrainz… |
| 3 | [`trakt`](./trakt.md) | Trakt | 📦 | `media/` | P0 | S | Needs-login | The Last.fm of TV and movies: a watch-history aggregator that media players (Plex, Kodi, Infuse, Emby) scrobble into automatically, plus manual check-ins. |
| 4 | [`claude-code`](./claude-code.md) | Claude Code | 📦 | `developer/` | P0 | S | 🔒 | Claude Code session history: every AI-assisted coding session, as plain JSONL in the home directory. |
| 5 | [`github`](./github.md) | GitHub | 📦 | `developer/` | P0 | S | Needs-login | The cloud half of developer activity: commits on repos you don't have cloned, PRs, issues, stars, and gists. |
| 6 | [`local-git`](./local-git.md) | Local Git Activity | 📦 | `developer/` | P0 | S | — | The commit history of every repository on the user's machine — the "what did I build" trail. |
| 7 | [`shell-history`](./shell-history.md) | Shell History | 📦 | `developer/` | P0 | S | 🔒 | The terminal command log — ~/.zsh_history and friends. |
| 8 | [`apple-contacts`](./apple-contacts.md) | Apple Contacts | 📦 | `contacts/` | P0 | M | Needs-David | The macOS address book — the anchor of Trove's whole person layer. |
| 9 | [`imap`](./imap.md) | IMAP Email (any provider) | 📦 | `correspondence/` | P0 | M | 🔒 Needs-login | The universal email protocol: one generic collector serves iCloud Mail, Yahoo, Zoho, Fastmail (as fallback), self-hosted, and any custom-domain mailbox —… |
| 10 | [`outlook`](./outlook.md) | Microsoft Outlook | 📦 | `correspondence/` | P0 | M | 🔒 Needs-login Needs-David | Microsoft email — Outlook.com personal, Hotmail, and Microsoft 365 work/school accounts — via the Graph API. |
| 11 | [`nws`](./nws.md) | National Weather Service | 📦 | `environment/` | P1 | S | — | The US National Weather Service's official public API. |
| 12 | [`simkl`](./simkl.md) | Simkl | 📦 | `media/` | P1 | S | Needs-login Needs-David | TV/film/anime watch tracker and universal scrobble hub — the Trakt alternative, with notably better anime ID mapping (AniDB/AniList). |
| 13 | [`todoist`](./todoist.md) | Todoist | 📦 | `tasks/` | P1 | S | Needs-login | One of the most widely used cross-platform task managers: projects, sections, labels, tasks with due dates, priorities, sub-tasks, reminders, and comments. |
| 14 | [`discord`](./discord.md) | Discord | 📦 | `correspondence/` | P1 | M | 🔒 | One of the highest-usage chat platforms (communities, gaming, increasingly group DMs). |
| 15 | [`mediaremote`](./mediaremote.md) | Mac Now Playing (MediaRemote) | 📋 | `media/` | P1 | M | Needs-sample Needs-David (parked) | Universal now-playing capture for the Mac itself: whatever any app tells macOS it is playing (Spotify desktop, Music, Safari video, IINA, …) surfaces… |
| 16 | [`garmin`](./garmin.md) | Garmin Connect | 📦 | `health/` | P1 | L | 🔒 | The dominant GPS sports-watch ecosystem; many users have years (often a decade+) of activity history here. |
| 17 | [`apple-voice-memos`](./apple-voice-memos.md) | Apple Voice Memos | 📦 | `voice/` | P1 | S | 🔒 Needs-David | Apple's built-in recorder. iCloud sync means iPhone memos land in the same Mac-local directory — so this one collector captures every voice note the user… |
| 18 | [`bear`](./bear.md) | Bear | 📦 | `notes/` | P1 | S | 🔒 Needs-David | Markdown notes app for Mac/iOS with a tag-based organization model, popular with writers and developers. |
| 19 | [`boardgamegeek`](./boardgamegeek.md) | BoardGameGeek | 📦 | `gaming/` | P1 | S | — | The canonical board-game database and community. |
| 20 | [`dropbox`](./dropbox.md) | Dropbox | 🧪 | `files/` | P1 | S | Needs-David | The most widely-installed third-party cloud drive. |
| 21 | [`facebook`](./facebook.md) | Facebook | 📦 | `social/` | P1 | S | 🔒 | The largest social network's comprehensive personal export: posts, photos/videos metadata, comments, reactions, friends, search history, marketplace,… |
| 22 | [`fathom`](./fathom.md) | Fathom | 🧪 | `meetings/` | P1 | S | 🔒 Needs-login | AI video notetaker that records calls (Zoom/Meet/Teams) and produces transcripts with speaker labels and timestamps, plus highlights and summaries. |
| 23 | [`exif-import`](./exif-import.md) | Image Files (EXIF) | 📦 | `photos/` | P1 | S | 🔒 | EXIF metadata extraction for image and video files managed *outside* Apple Photos — drag a folder of JPEGs/HEICs, import a camera SD card, and Trove indexes… |
| 24 | [`readwise`](./readwise.md) | Readwise + Readwise Reader | 📦 | `reading/` | P1 | S | Needs-login | The reading hub: Readwise aggregates highlights from Kindle, Apple Books, web articles, and PDFs into one account; Reader is its companion read-later app… |
| 25 | [`toggl-track`](./toggl-track.md) | Toggl Track | 📦 | `time-entries/` | P1 | S | Needs-login | One of the most popular manual time trackers: the user starts/stops timers or logs entries against projects, tasks, tags, and clients. First collector in the `time-entries` domain (contract ratified). |
| 26 | [`airnow`](./airnow.md) | AirNow (EPA AQI) | 📦 | `environment/` | P1 | S | Needs-login | AirNow is the US EPA's official air-quality service: authoritative AQI from actual ground-level monitors (PM2.5, PM10, ozone, CO, NO2, SO2), plus forecast… |
| 27 | [`asana`](./asana.md) | Asana | 📦 | `tasks/` | P1 | S | Needs-login | Asana is a widely-used task / project-management service common in professional teams. |
| 28 | [`bitcoin`](./bitcoin.md) | Bitcoin Wallet (Blockstream) | 📦 | `finance/` | P1 | S | 🔒 Needs-login | Full transaction history for any Bitcoin address via Blockstream's free, keyless Esplora API. First collector in the `finance-purchases` domain (contract ratified; on-chain value transfers as dated purchase line items). |
| 29 | [`chess-com`](./chess-com.md) | Chess.com | 📦 | `gaming/` | P1 | S | Needs-login | The largest online chess platform. Every game a user plays is published with full PGN (the complete move record), time control, result, ratings, opening,… |
| 30 | [`day-one`](./day-one.md) | Day One | 📦 | `notes/` | P1 | S | Needs-sample | The dominant journaling app (~10M users). Entries are markdown with rich first-class metadata: creation/modified timestamps, location, weather, tags, and… |
| 31 | [`drafts`](./drafts.md) | Drafts | 📦 | `notes/` | P1 | S | — | Drafts (Agiletortoise) is a "capture first, act later" notes app for Mac and iOS: everything starts in one frictionless text field, then gets tagged, filed,… |
| 32 | [`fastmail`](./fastmail.md) | Fastmail | 📦 | `correspondence/` | P1 | S | 🔒 Needs-login | A privacy-focused paid email provider, popular with exactly the audience a local-first vault attracts. |
| 33 | [`fidelity`](./fidelity.md) | Fidelity | 🧪 | `finance/` | P1 | S | Needs-sample | Largest US retail brokerage by assets. Trade history, settlements, and position snapshots for brokerage/retirement accounts. |
| 34 | [`fireflies`](./fireflies.md) | Fireflies.ai | 📦 | `meetings/` | P1 | S | 🔒 Needs-login | AI meeting assistant that joins meetings as a bot participant, records, and transcribes. |
| 35 | [`github-copilot`](./github-copilot.md) | GitHub Copilot | 📦 | `developer/` | P1 | S | 🔒 | GitHub Copilot Chat conversations as stored locally by VS Code — the largest-userbase AI coding assistant, so the broadest-reach AI-sessions source after… |
| 36 | [`gitlab`](./gitlab.md) | GitLab | 📦 | `developer/` | P1 | S | 🔒 Needs-login | The second developer platform after GitHub: commits, merge requests, issues, and the user activity event feed, for both gitlab.com and self-hosted instances. |
| 37 | [`google-drive`](./google-drive.md) | Google Drive | 🧪 | `files/` | P1 | S | — | Google's cloud drive via the Drive for Desktop app. |
| 38 | [`google-voice`](./google-voice.md) | Google Voice | 📦 | `correspondence/` | P1 | S | 🔒 | Google's virtual phone number service: calls, SMS, and transcribed voicemails. |
| 39 | [`granola`](./granola.md) | Granola | 📦 | `meetings/` | P1 | S | 🔒 Needs-login | AI meeting-notes app: joins/listens to meetings and produces AI summaries, and (on paid plans) full transcripts with speaker attribution. |
| 40 | [`icloud-drive`](./icloud-drive.md) | iCloud Drive | 🧪 | `files/` | P1 | S | — | Apple's cloud file storage, on by default for most Mac users. |
| 41 | [`imdb`](./imdb.md) | IMDb | 📦 | `media/` | P1 | S | — | The default movie-rating tool for a huge share of casual film watchers — many users have years of star ratings on IMDb and nowhere else. |
| 42 | [`instagram`](./instagram.md) | Instagram | 📦 | `social/` | P1 | S | 🔒 | Meta's photo/video network. The official JSON export is one ZIP covering posts (captions, timestamps, location tags), stories, reels, DMs,… |
| 43 | [`lichess`](./lichess.md) | Lichess | 📦 | `gaming/` | P1 | S | — | The open-source, nonprofit online chess platform — second only to Chess.com in reach, and the ethos match for Trove (open API, no ads, no paywalls on data). |
| 44 | [`linear`](./linear.md) | Linear | 📦 | `tasks/` | P1 | S | — | Linear is a fast, opinionated issue tracker popular with software teams and indie developers. |
| 45 | [`linkedin`](./linkedin.md) | LinkedIn | 📦 | `contacts/` | P1 | S | 🔒 | The professional network — where most professional contacts originate. |
| 46 | [`logseq`](./logseq.md) | Logseq | 📦 | `notes/` | P1 | S | — | Logseq is a local-first outliner / knowledge base storing a "graph" as a folder of plain markdown (.md, optionally .org) files on disk. |
| 47 | [`netflix`](./netflix.md) | Netflix | 📦 | `media/` | P1 | S | — | The largest streaming video service, and the first streaming import worth building: the only major streamer with an *instant*, official viewing history export. |
| 48 | [`noaa-swpc`](./noaa-swpc.md) | NOAA Space Weather | 📦 | `environment/` | P1 | S | — | NOAA's Space Weather Prediction Center — near-real-time geomagnetic and solar data: the planetary Kp index, geomagnetic storm alerts, and the OVATION aurora… |
| 49 | [`obsidian`](./obsidian.md) | Obsidian | 📦 | `notes/` | P1 | S | — | Obsidian is a popular local-first Markdown knowledge base. |
| 50 | [`onedrive`](./onedrive.md) | OneDrive | 📦 | `files/` | P1 | S | — | Microsoft's cloud drive, ubiquitous for anyone in the Microsoft 365 ecosystem (less common than iCloud/Dropbox on personal Macs, but a large population). |
| 51 | [`prime-video`](./prime-video.md) | Prime Video | 📦 | `media/` | P1 | S | Needs-sample | Amazon's streaming video service — bundled with Prime, so the audience is enormous and most users have history they've never seen. |
| 52 | [`protonmail`](./protonmail.md) | ProtonMail | 📦 | `correspondence/` | P1 | S | 🔒 Needs-sample | Proton's encrypted email service — disproportionately popular with exactly the privacy-conscious audience Trove targets. |
| 53 | [`robinhood`](./robinhood.md) | Robinhood | 📋 | `finance/` | P1 | S | 🔒 | Hugely popular US retail brokerage, especially with younger investors — stocks, options, and crypto trades. |
| 54 | [`steam`](./steam.md) | Steam | 📦 | `gaming/` | P1 | S | Needs-login | The dominant PC game store/launcher. The Web API is the gold standard of the gaming domain — official, free, stable, no scraping — and yields the user's… |
| 55 | [`things`](./things.md) | Things 3 | 📦 | `tasks/` | P1 | S | — | Things 3 (Cultured Code) is the highest-value local-first task manager on the Mac: a polished GTD app used heavily by individuals. |
| 56 | [`threads`](./threads.md) | Threads | 📦 | `social/` | P1 | S | Needs-sample | Meta's microblogging platform (the X competitor), attached to Instagram accounts. |
| 57 | [`usgs-earthquakes`](./usgs-earthquakes.md) | USGS Earthquakes | 📦 | `environment/` | P1 | S | — | The USGS Earthquake Catalog is the authoritative, government-maintained global record of seismic events. |
| 58 | [`usno`](./usno.md) | USNO Astronomy | 📦 | `environment/` | P1 | S | — | The US Naval Observatory Astronomical Applications API is the authoritative, keyless source for daily solar and lunar data: sunrise, sunset, solar noon, civil twilight, moonrise/set, phase, illumination. First collector to bind the environment *almanac* shape. |
| 59 | [`vanguard`](./vanguard.md) | Vanguard | 🧪 | `finance/` | P1 | S | 🔒 Needs-sample | The largest US retirement/mutual-fund custodian — Vanguard dominates 401k and IRA assets, so for many users it holds the bulk of their net worth. |
| 60 | [`vcard`](./vcard.md) | vCard Import (.vcf) | 📦 | `contacts/` | P1 | S | — | The universal contact interchange format — every contact service exports to .vcf: iCloud.com, Google Contacts / Takeout (contacts.vcf), Outlook, any CardDAV… |
| 61 | [`waqi`](./waqi.md) | World Air Quality Index | 📦 | `environment/` | P1 | S | — | WAQI / aqicn.org aggregates ground-monitor air quality from 10,000+ stations worldwide, returning AQI, dominant pollutant, and station name from a single… |
| 62 | [`youtube-music`](./youtube-music.md) | YouTube Music | 📦 | `media/` | P1 | S | — | Google's music-streaming service. Listening history is complete back to account creation but has no API — Takeout is the only path. |
| 63 | [`apple-mail`](./apple-mail.md) | Apple Mail | 🧪 | `correspondence/` | P1 | M | 🔒 | The Mail.app local store: every account the user has configured (iCloud, Gmail, Outlook, custom IMAP) keeps a cached copy on disk. |
| 64 | [`apple-notes`](./apple-notes.md) | Apple Notes | 📦 | `notes/` | P1 | M | — | Apple's built-in Notes app — where most Mac/iPhone users keep freeform notes, checklists, and clippings. |
| 65 | [`apple-photos`](./apple-photos.md) | Apple Photos | 📦 | `photos/` | P1 | M | 🔒 | The richest local photo source on a Mac. Photos.sqlite holds per-asset metadata for the whole library — timestamps, GPS, favorites, albums, face/ people… |
| 66 | [`caldav`](./caldav.md) | CalDAV (any server) | 📦 | `calendar/` | P1 | M | — | The protocol under iCloud Calendar, Fastmail, Nextcloud, Proton Calendar (via Bridge, 2024+), Radicale/Baikal self-hosting, and Google Calendar's alternate… |
| 67 | [`schwab`](./schwab.md) | Charles Schwab | 📦 | `finance/` | P1 | M | 🔒 Needs-login | One of the largest US retail brokerages (equities, options, mutual funds, ETFs, retirement accounts), with an official, free, individual-developer API —… |
| 68 | [`coinbase`](./coinbase.md) | Coinbase | 📦 | `finance/` | P1 | M | 🔒 | The largest US crypto exchange. Buys, sells, sends, receives, staking, and Advanced Trade (ex-Pro) orders. |
| 69 | [`cursor`](./cursor.md) | Cursor | 📦 | `developer/` | P1 | M | 🔒 | Cursor's AI chat and agent session history — the AI-pairing trail for one of the most popular AI IDEs. |
| 70 | [`dexcom`](./dexcom.md) | Dexcom | 📦 | `health/medical/` | P1 | M | Needs-login | Dexcom makes the dominant continuous glucose monitors (G6, G7, ONE, ONE+): a sensor reading blood glucose every 5 minutes, ~288 readings/day. First collector in the `health-medical` domain — binds `health-medical.observation`. |
| 71 | [`ethereum`](./ethereum.md) | Ethereum Wallet (Etherscan) | 📦 | `finance/` | P1 | M | 🔒 | On-chain transaction history for Ethereum and EVM-compatible chains (Polygon, Arbitrum, Optimism, Base, …) via the Etherscan API. |
| 72 | [`facebook-messenger`](./facebook-messenger.md) | Facebook Messenger | 📦 | `correspondence/` | P1 | M | 🔒 | One of the most used messengers globally. The official Meta export is well-structured JSON covering all DMs and group chats (both sides of every… |
| 73 | [`fitbit`](./fitbit.md) | Fitbit | 📦 | `health/` | P1 | M | — | Fitness trackers with tens of millions of users; many early wearable adopters have years of Fitbit history that never reached Apple Health. |
| 74 | [`google-chat`](./google-chat.md) | Google Chat | 📦 | `correspondence/` | P1 | M | 🔒 | Google's team-chat product (successor to Hangouts), ubiquitous in Google Workspace orgs and present on every personal Google account. |
| 75 | [`google-takeout`](./google-takeout.md) | Google Takeout (My Activity) | 📦 | `browser-searches` | P1 | M | — | Google's data-export archive. Built: the My Activity **Search** query log → `browser-searches` (first collector of the domain). YouTube watch history is a separate slice, deferred. |
| 76 | [`google-timeline`](./google-timeline.md) | Google Timeline | 📦 | `location/` | P1 | M | 🔒 Needs-sample | Google's continuous location-history trail — the place visits and movement segments Google Maps records as you go about your day. Built: **first collector of the `location/` domain** (binds the `Fix` contract); the Timeline.json import scaffold + lossless raw preservation are live, the trail-row mapping is parked for a real per-platform export. |
| 77 | [`interactive-brokers`](./interactive-brokers.md) | Interactive Brokers | 📦 | `finance/` | P1 | M | 🔒 | Interactive Brokers — the brokerage of choice for active traders and international users; covers equities, options, futures, forex, and crypto. |
| 78 | [`jira`](./jira.md) | Jira | 📦 | `tasks/` | P1 | M | — | Atlassian's issue tracker / project-management tool, ubiquitous in engineering and corporate teams. |
| 79 | [`kraken`](./kraken.md) | Kraken | 📦 | `finance/` | P1 | M | 🔒 | Major US crypto exchange — largest by volume for many asset pairs. |
| 80 | [`labcorp`](./labcorp.md) | Labcorp | 📦 | `health/` | P1 | M | 🔒 | Labcorp is the other half of the US lab duopoly with Quest. |
| 81 | [`nasa-firms`](./nasa-firms.md) | NASA FIRMS Wildfire | 📦 | `environment/` | P1 | M | — | NASA's Fire Information for Resource Management System — satellite active-fire detections (thermal anomalies) from MODIS and VIIRS, available globally… |
| 82 | [`outlook-calendar`](./outlook-calendar.md) | Outlook Calendar | 📦 | `calendar/` | P1 | M | — | Microsoft's calendar, read via the Graph API — covers both personal Outlook.com / Microsoft accounts and M365 / enterprise tenants. |
| 83 | [`quest-diagnostics`](./quest-diagnostics.md) | Quest Diagnostics | 📦 | `health/` | P1 | M | 🔒 | Quest is the largest US lab network; most Americans who've had bloodwork have results in MyQuest. |
| 84 | [`reddit`](./reddit.md) | Reddit | 📦 | `social/` | P1 | M | 🔒 | The dominant forum/communities platform. A heavy user's comment and submission history is a longitudinal record of interests, opinions, and communities over… |
| 85 | [`spotify`](./spotify.md) | Spotify | 📋 | `media/` | P1 | M | — | The dominant music streaming service. Its GDPR "Extended Streaming History" export is the gold standard of listening data: complete lifetime history with… |
| 86 | [`strava`](./strava.md) | Strava | 📦 | `health/` | P1 | M | 🔒 Needs-login | The de facto social home for GPS workouts, with an enormous user base. |
| 87 | [`telegram`](./telegram.md) | Telegram | 📦 | `correspondence/` | P1 | M | 🔒 Needs-login | One of the largest messengers worldwide. Telegram Desktop ships an official, full-history export with a stable documented JSON schema — unusually good for a… |
| 88 | [`apple-voicemail`](./apple-voicemail.md) | Visual Voicemail (iPhone backup) | 📦 | `voice/` | P1 | M | 🔒 Needs-sample | iPhone Visual Voicemail, surfaced via the local Finder/iTunes backup on the Mac. |
| 89 | [`whoop`](./whoop.md) | WHOOP | 📦 | `health/` | P1 | M | Needs-login Needs-sample | Screenless fitness/recovery band with a popular subscriber base. |
| 90 | [`withings`](./withings.md) | Withings | 📦 | `health/` | P1 | M | 🔒 Needs-login | Connected-health device maker: smart scales (Body/Body+/Body Cardio/Body Scan), ScanWatch, blood-pressure monitors, sleep mat, thermometer — all under one API. |
| 91 | [`x-twitter`](./x-twitter.md) | X (Twitter) | 📦 | `social/` | P1 | M | 🔒 | The user's complete Twitter/X history in one official ZIP: full tweet text (full_text, untruncated), media, likes, followers/following, lists, ad… |
| 92 | [`zoom`](./zoom.md) | Zoom | 📦 | `meetings/` | P1 | M | 🔒 Needs-login | The dominant work meeting platform. Two distinct mechanisms, one entry: cloud recordings + VTT transcripts + AI Companion summaries via the REST API (Pro… |
| 93 | [`epic-mychart`](./epic-mychart.md) | Epic MyChart | 📦 | `health/` | P1 | L | 🔒 Needs-login | Epic is the dominant US hospital EHR; MyChart is its patient portal. |
| 94 | [`smart-on-fhir`](./smart-on-fhir.md) | Medical Records (SMART on FHIR) | 📦 | `health/` | P1 | L | 🔒 | The generic SMART-on-FHIR patient-access client — the canonical Mac-native path for structured clinical records in the US. |
| 95 | [`amazon-alexa`](./amazon-alexa.md) | Amazon Alexa | 📦 | `home/` | P2 | S | 🔒 | Amazon Alexa logs every voice interaction with Echo/Alexa devices: the timestamp, the device, the spoken command transcription, and Alexa's response text. |
| 96 | [`habitica`](./habitica.md) | Habitica | 📦 | `habits/` | P2 | S | Needs-login | Gamified habit-and-task tracker: habits, dailies, and to-dos earn XP, gold, and streaks in an RPG frame. |
| 97 | [`23andme`](./23andme.md) | 23andMe | 📦 | `health/` | P2 | S | 🔒 | Consumer genetic-testing service. The "Download Raw Data" file is a tab-delimited table of the user's measured SNP genotypes — the foundational layer for… |
| 98 | [`alfred`](./alfred.md) | Alfred Clipboard | 📦 | `developer/` | P2 | S | 🔒 | Alfred's Powerpack clipboard history: everything the user copied, with timestamp and source app, sitting in a plain SQLite database. |
| 99 | [`ambient-weather`](./ambient-weather.md) | Ambient Weather | 📦 | `home/` | P2 | S | Needs-login | Ambient Weather personal weather stations (WS-2902 family and kin), read through the ambientweather.net cloud. First collector in the `home` domain — binds `home.reading`. |
| 100 | [`pinboard`](./pinboard.md) | Pinboard | 📦 | `reading/` | P2 | S | — | Minimalist paid bookmarking service beloved by a small, long-tenured audience — dedicated users hold 15+ years of bookmark history with tags and descriptions. |
| 101 | [`skype`](./skype.md) | Skype (archival) | 📦 | `correspondence/` | P2 | S | 🔒 | Skype shut down in May 2025 (accounts migrated to Teams). |
| 102 | [`tldv`](./tldv.md) | tl;dv | 📦 | `meetings/` | P2 | S | 🔒 Needs-login | Meeting recorder/notetaker covering Zoom, Google Meet, and Teams with 40+ language support; large integration surface via Zapier/n8n. |
| 103 | [`google-meet`](./google-meet.md) | Google Meet | 📦 | `meetings/` | P2 | M | 🔒 | Google's meeting platform. When the organizer enables transcription, Meet produces utterance-level transcripts retrievable via the Meet REST API v2 and a… |
| 104 | [`home-assistant`](./home-assistant.md) | Home Assistant | 📦 | `home/` | P2 | M | — | The self-hosted smart-home hub. For users who already run Home Assistant, one integration pulls state history for every device HA knows about — Zigbee,… |
| 105 | [`krisp`](./krisp.md) | Krisp | 📦 | `meetings/` | P2 | M | 🔒 Needs-sample | Krisp is primarily a system-wide noise-cancellation tool that added meeting notetaking (transcripts, notes, outlines) as a secondary feature. |
| 106 | [`macos-microphone`](./macos-microphone.md) | Mac Microphone Ambient Sound | 📋 | `environment/` | P2 | M | 🔒 Needs-David | Samples the built-in Mac microphone to log an *ambient loudness level* (dB) — quiet office vs. |
| 107 | [`overland`](./overland.md) | Overland (iOS GPS Logger) | 📋 | `location/` | P2 | M | 🔒 | Overland is the canonical open-source always-on iOS GPS logger (IndieWeb favorite). |
| 108 | [`owntracks`](./owntracks.md) | OwnTracks | 📦 | `location/` | P2 | M | 🔒 | OwnTracks is an open-source iOS/Android GPS logger, the other half of the self-hosted-location pair with Overland. |
| 109 | [`pocket-casts`](./pocket-casts.md) | Pocket Casts | 📦 | `media/` | P2 | M | Needs-login | A major cross-platform podcast app (owned by Automattic). |
| 110 | [`ring`](./ring.md) | Ring | 📦 | `home/` | P2 | M | Needs-login | Doorbell and security-camera event logs from Ring (Amazon): who rang, when motion was detected, which device fired. |
| 111 | [`smartcar`](./smartcar.md) | Smartcar | 📦 | `location/` | P2 | M | 🔒 | Connected-car API broker (successor to Automatic) that normalizes 40+ OEMs — Tesla, GM/Chevy/GMC, Ford, BMW, Hyundai/Kia, Toyota, VW, Mercedes-Benz,… |
| 112 | [`coros`](./coros.md) | COROS | 📦 | `health/` | P2 | L | 🔒 | GPS sports watches with a growing base among runners and triathletes. |
| 113 | [`signal`](./signal.md) | Signal | 📦 | `correspondence/` | P2 | L | 🔒 | The privacy-first messenger. By design it has no export feature and no API — the only access path is Signal Desktop's local SQLCipher-encrypted SQLite DB. |
| 114 | [`tesla`](./tesla.md) | Tesla (vehicle) | 📦 | `location/` | P2 | L | 🔒 Needs-login | Tesla's Fleet API exposes a connected vehicle's state, including location. |
| 115 | [`airbnb`](./airbnb.md) | Airbnb | 📦 | `travel/` | P2 | S | 🔒 Needs-sample | Airbnb is a lodging marketplace; guest booking history records where a user stayed, the dates, the city/country, the confirmation code, and the amount paid. |
| 116 | [`wakatime`](./wakatime.md) | WakaTime | 📦 | `activity/` | P2 | S | Needs-login | The standard cloud coding-time tracker: editor plugins send heartbeats (file, project, language, editor, OS) to WakaTime, which aggregates them into… |
| 117 | [`airthings`](./airthings.md) | Airthings | 📋 | `home/` | P2 | S | — | Airthings (Wave / Wave Plus / View) is an indoor air quality monitor line whose standout is radon — continuous Bq/m³ measurement that no other consumer… |
| 118 | [`amazing-marvin`](./amazing-marvin.md) | Amazing Marvin | 📋 | `tasks/` | P2 | S | Needs-login | Amazing Marvin is a highly customizable to-do/productivity app with an enthusiastic ADHD/power-user base. |
| 119 | [`ancestrydna`](./ancestrydna.md) | AncestryDNA | 📦 | `health/` | P2 | S | 🔒 | Consumer genetic-testing service focused on ethnicity estimates and family trees. |
| 120 | [`apple-card`](./apple-card.md) | Apple Card, Cash & Savings | 📦 | `finance/` | P2 | S | 🔒 Needs-sample | Apple's consumer financial products: Apple Card (credit card), Apple Cash (P2P balance), and Apple Savings. |
| 121 | [`awair`](./awair.md) | Awair | 📋 | `home/` | P2 | S | — | Awair (Element / 2nd Edition / Omni) is a popular prosumer indoor air quality monitor: CO2, VOC, PM2.5, temperature, humidity, plus Awair's composite score. |
| 122 | [`bearable`](./bearable.md) | Bearable | 📦 | `health/` | P2 | S | 🔒 | Symptom/mood/medication tracker. The user logs mood ratings, pain/fatigue scores, symptoms, medications taken, and lifestyle factors per entry with timestamps. |
| 123 | [`bitbucket`](./bitbucket.md) | Bitbucket | 📦 | `developer/` | P2 | S | — | Atlassian's git hosting platform. Declining market share versus GitHub/GitLab but still common in Atlassian-shop workplaces, so some users' "what did I… |
| 124 | [`bumble`](./bumble.md) | Bumble | 📋 | `social/` | P2 | S | 🔒 Needs-sample Needs-David | Dating app (the "women message first" one). The export covers account data, match history, and full message threads — relationship-formation history that… |
| 125 | [`cal-com`](./cal-com.md) | Cal.com | 📦 | `calendar/` | P2 | S | — | Open-source Calendly alternative for booking pages and scheduled meetings. |
| 126 | [`calendly`](./calendly.md) | Calendly | 📦 | `calendar/` | P2 | S | — | Scheduling tool: people book time on the user's Calendly links and the booked meetings accumulate as a scheduling history. |
| 127 | [`capacities`](./capacities.md) | Capacities | 📦 | `notes/` | P2 | S | — | Object-based notes / personal-knowledge-base app. |
| 128 | [`cash-app`](./cash-app.md) | Cash App | 📦 | `finance/` | P2 | S | 🔒 | Cash App account history — peer payments, Cash App Card purchases, and Bitcoin buys/sells — via the official all-time CSV export. |
| 129 | [`chatgpt`](./chatgpt.md) | ChatGPT | 📦 | `developer/` | P2 | S | 🔒 | OpenAI's chat assistant — for many users the single largest record of what they were thinking about, asking, and working on. |
| 130 | [`clay`](./clay.md) | Clay (Mesh) | 📦 | `contacts/` | P2 | S | Needs-sample | Clay (partially rebranded "Mesh" at clay.earth) is a personal CRM that enriches the user's address book by pulling from email, calendar, LinkedIn, Twitter,… |
| 131 | [`clockify`](./clockify.md) | Clockify | 📦 | `time-entries/` | P2 | S | — | Time-tracking tool: the user manually starts/stops timers or logs entries against projects and tasks. |
| 132 | [`craft`](./craft.md) | Craft | 📋 | `notes/` | P2 | S | — | Polished document/notes app (Mac App Store, CloudKit backend). |
| 133 | [`cronometer`](./cronometer.md) | Cronometer | 📦 | `health/` | P2 | S | — | Cronometer is the nutrition tracker for people who care about *micronutrients* — widely regarded as having the most complete micronutrient database of any… |
| 134 | [`dex`](./dex.md) | Dex (Personal CRM) | 📋 | `contacts/` | P2 | S | Needs-login | A personal CRM oriented around LinkedIn: it auto-logs interactions from LinkedIn, Gmail, Calendar, iMessage, and Twitter, and layers on tags, notes,… |
| 135 | [`evernote`](./evernote.md) | Evernote | 📦 | `notes/` | P2 | S | — | Evernote is one of the original cloud note-taking apps — notebooks, web clips, scanned documents, and tagged notes accumulated by a large legacy user base… |
| 136 | [`flighty`](./flighty.md) | Flighty | 📦 | `travel/` | P2 | S | Needs-sample | Flight-tracker app (iOS-first, with a Mac App Store build). |
| 137 | [`goodreads`](./goodreads.md) | Goodreads | 📦 | `media/` | P2 | S | Needs-sample | The dominant social-reading service (Amazon-owned): shelves, star ratings, reviews, and read-dates for tens of millions of readers, often going back 15+ years. |
| 138 | [`google-keep`](./google-keep.md) | Google Keep | 📦 | `notes/` | P2 | S | Needs-sample | Google's lightweight notes/checklist app. Holds short notes, checklists, colors, pins/archives, and image attachments. |
| 139 | [`google-maps`](./google-maps.md) | Google Maps Saved Places | 📦 | `location/` | P2 | S | 🔒 | A user's curated Google Maps places — Starred, Labeled (Home/Work), Want to go, and other saved lists. |
| 140 | [`habitify`](./habitify.md) | Habitify | 📋 | `habits/` | P2 | S | Needs-login | Cross-platform habit tracker with a clean, minimalist log. |
| 141 | [`hardcover`](./hardcover.md) | Hardcover | 📦 | `reading/` | P2 | S | Needs-login | A rising Goodreads alternative that — unlike Goodreads, StoryGraph, and Literal — ships a real, officially documented API: the same GraphQL endpoint its own… |
| 142 | [`harvest`](./harvest.md) | Harvest | 📦 | `time-entries/` | P2 | S | Needs-login | Time-tracking and invoicing tool popular with freelancers and agencies: clock hours against clients, projects, and tasks. |
| 143 | [`hinge`](./hinge.md) | Hinge | 📋 | `social/` | P2 | S | 🔒 Needs-David | Major dating app ("designed to be deleted") owned by Match Group. |
| 144 | [`hypothesis`](./hypothesis.md) | Hypothesis | 📦 | `reading/` | P2 | S | Needs-login | Hypothesis is the open-source (AGPL) web-annotation layer: users highlight and annotate any web page or PDF, publicly or in private groups. |
| 145 | [`irc`](./irc.md) | IRC Logs (ZNC / WeeChat / Irssi) | 📦 | `correspondence/` | P2 | S | 🔒 Needs-sample | Plaintext chat logs left on disk by the three IRC setups still in use in 2026: a ZNC bouncer, WeeChat, and Irssi. |
| 146 | [`jetbrains`](./jetbrains.md) | JetBrains IDEs | 📦 | `developer/` | P2 | S | Needs-sample | The JetBrains IDE family (IntelliJ IDEA, WebStorm, PyCharm, GoLand, etc.) — the most popular non-VS-Code IDE line. |
| 147 | [`kindle`](./kindle.md) | Kindle Highlights | 📦 | `reading/` | P2 | S | — | Highlights, notes, and bookmarks made on a physical Kindle e-reader, accumulated in one plain-text file on the device. |
| 148 | [`crypto-tax-exports`](./crypto-tax-exports.md) | Koinly / CoinTracker Exports | 📦 | `finance/` | P2 | S | 🔒 | Koinly and CoinTracker are crypto tax aggregators: users wire up all their exchanges and wallets once, and the service produces a normalized,… |
| 149 | [`levels-health`](./levels-health.md) | Levels | 📦 | `health/` | P2 | S | 🔒 Needs-login | Levels is a metabolic-health subscription app ($200+/yr) that pairs a continuous glucose monitor (CGM) with food logging and computes proprietary Zones… |
| 150 | [`libby`](./libby.md) | Libby / OverDrive | 📦 | `media/` | P2 | S | — | Libby (by OverDrive) is the dominant app for borrowing ebooks and audiobooks from public libraries. |
| 151 | [`lifesum`](./lifesum.md) | Lifesum | 📦 | `health/` | P2 | S | Needs-sample | Popular nutrition/meal-logging app (strong in Europe). |
| 152 | [`macos-downloads`](./macos-downloads.md) | macOS Downloads | 🧪 | `files/` | P2 | S | — | macOS's quarantine system logs every file downloaded by Safari, Chrome, Firefox, Mail, and any quarantine-aware app into a single home-dir SQLite database —… |
| 153 | [`macrofactor`](./macrofactor.md) | MacroFactor | 📦 | `health/` | P2 | S | Needs-login | MacroFactor is a popular adherence-neutral macro/calorie tracker with an adaptive-expenditure algorithm (TDEE estimated from intake + weight trend). |
| 154 | [`medisafe`](./medisafe.md) | Medisafe | 📦 | `health/` | P2 | S | 🔒 Needs-login | Medication-reminder app; its export carries what almost nothing else does: adherence — doses taken vs. |
| 155 | [`monica`](./monica.md) | Monica (Personal CRM) | 📋 | `contacts/` | P2 | S | — | Open-source personal CRM with a niche but dedicated user base, cloud or self-hosted. |
| 156 | [`motion`](./motion.md) | Motion | 📋 | `tasks/` | P2 | S | Needs-login | Motion is an AI calendar/task app: you add tasks with durations, deadlines, and priorities, and it auto-schedules them into open calendar slots. |
| 157 | [`myfitnesspal`](./myfitnesspal.md) | MyFitnessPal | 📦 | `health/` | P2 | S | Needs-login | MyFitnessPal is the biggest consumer food-logging app. |
| 158 | [`myflightradar24`](./myflightradar24.md) | myFlightRadar24 | 📦 | `travel/` | P2 | S | — | myFlightRadar24 is Flightradar24's personal flight-logbook feature: users record the flights they've taken and export the log as a CSV. |
| 159 | [`netatmo`](./netatmo.md) | Netatmo Weather Station | 📦 | `home/` | P2 | S | — | Popular prosumer personal weather station: outdoor temperature, humidity, pressure, rain, wind, plus indoor temperature, humidity, CO2 and noise from the… |
| 160 | [`nightscout`](./nightscout.md) | Nightscout | 📦 | `health/` | P2 | S | 🔒 | Nightscout is the self-hosted, open-source CGM aggregator run by the T1D community: users deploy it themselves (cloud host or local machine) and feed it… |
| 161 | [`noaa-co-ops`](./noaa-co-ops.md) | NOAA Tides & Currents | 📦 | `environment/` | P2 | S | — | NOAA's Center for Operational Oceanographic Products and Services — tide predictions and water-level/met observations from 3,000+ US coastal stations. |
| 162 | [`omnivore`](./omnivore.md) | Omnivore (historical import) | 📦 | `reading/` | P2 | S | — | Omnivore was a beloved open-source read-later app, shut down November 2024 after the ElevenLabs acquihire; all hosted data was deleted. |
| 163 | [`password-manager`](./password-manager.md) | Password Manager Metadata (1Password, Bitwarden) | 📋 | `files/` | P2 | S | 🔒 Needs-David | Password managers hold the canonical inventory of every service and account a person has: item names, categories, URLs, tags, notes, creation/modified dates. |
| 164 | [`paypal`](./paypal.md) | PayPal | 📦 | `finance/` | P2 | S | 🔒 Needs-sample | PayPal account activity — purchases, peer payments, refunds, balance transfers — via the official CSV activity download. |
| 165 | [`pharmacy-prescriptions`](./pharmacy-prescriptions.md) | Pharmacy Prescriptions | 📦 | `health/` | P2 | S | 🔒 Needs-sample Needs-David | Your prescription/medication history. There is no consumer-facing pharmacy API in 2026 — the chains' developer programs are B2B refill-ordering, not… |
| 166 | [`philips-hue`](./philips-hue.md) | Philips Hue | 📦 | `home/` | P2 | S | Needs-login | Philips Hue is the dominant consumer smart-lighting system: a LAN bridge that controls lights, plus Hue motion/temperature/daylight sensors, rooms, zones,… |
| 167 | [`pinterest`](./pinterest.md) | Pinterest | 📦 | `social/` | P2 | S | Needs-sample | Visual bookmarking: boards of saved pins representing the user's interests, plans, and taste over time. |
| 168 | [`pocket`](./pocket.md) | Pocket (historical import) | 📦 | `reading/` | P2 | S | Needs-sample | Pocket was the dominant read-later service until Mozilla shut it down on July 8, 2025; the export portal and API closed November 12, 2025. |
| 169 | [`raindrop`](./raindrop.md) | Raindrop.io | 📦 | `reading/` | P2 | S | — | Bookmark manager with collections, tags, and notes — one of the main landing spots for users displaced by the Pocket shutdown (July 2025), so the user base… |
| 170 | [`read-ai`](./read-ai.md) | Read.ai | 📦 | `meetings/` | P2 | S | 🔒 Needs-login Needs-David | AI meeting notetaker popular in enterprise Zoom/Teams/Google Meet workflows: joins meetings, produces transcripts, summaries, action items, and speaker… |
| 171 | [`reflect`](./reflect.md) | Reflect | 📦 | `notes/` | P2 | S | Needs-sample Needs-David | Reflect is a networked-thought / daily-notes app (the Roam/Obsidian lineage), end-to-end encrypted by design. |
| 172 | [`retroachievements`](./retroachievements.md) | RetroAchievements | 📦 | `gaming/` | P2 | S | Needs-login | The retro-gaming achievement service: emulator users earn community-authored achievements across classic consoles (NES through PS2-era). |
| 173 | [`roam`](./roam.md) | Roam Research | 📦 | `notes/` | P2 | S | — | Roam Research is the original networked-thought outliner — pages and nested blocks with bidirectional links, daily notes, and a graph database underneath. |
| 174 | [`shazam`](./shazam.md) | Shazam | 📦 | `media/` | P2 | S | — | Apple's music-identification app. Every "what song is this?" moment is a timestamped discovery event — a log of when and where the user *encountered* music,… |
| 175 | [`simplenote`](./simplenote.md) | Simplenote | 📦 | `notes/` | P2 | S | — | Simplenote is Automattic's free, minimalist plain-text note app — fast sync, tags, no formatting overhead. |
| 176 | [`snapchat`](./snapchat.md) | Snapchat | 📦 | `correspondence/` | P2 | S | 🔒 | Ephemeral messaging + photo app. Ephemeral snap *content* is gone by design — the export yields snap metadata only — but two assets matter: Memories… |
| 177 | [`standard-notes`](./standard-notes.md) | Standard Notes | 📦 | `notes/` | P2 | S | — | End-to-end-encrypted notes app (open source, AGPL; self-hostable sync server). |
| 178 | [`stoic`](./stoic.md) | Stoic | 📦 | `notes/` | P2 | S | — | iOS-first journaling app (Mac app arrived 2025) blending guided journaling with mood/metric tracking. |
| 179 | [`storygraph`](./storygraph.md) | StoryGraph | 📦 | `media/` | P2 | S | — | The de-facto Goodreads replacement: an independent (non-Amazon) social-reading tracker known for mood/pace stats and half-star ratings, growing fast since 2020. |
| 180 | [`stripe`](./stripe.md) | Stripe Billing | 📋 | `finance/` | P2 | S | 🔒 Needs-David | Stripe is the billing backend behind a huge share of SaaS subscriptions — but most consumers only ever see it as an email receipt. |
| 181 | [`substack`](./substack.md) | Substack | 📋 | `social/` | P2 | S | Needs-David | Newsletter publishing. For users who write on Substack, the export is their complete published archive plus audience/revenue stats. |
| 182 | [`sunrise-sunset`](./sunrise-sunset.md) | Sunrise-Sunset.org | 📦 | `environment/` | P2 | S | — | A keyless public REST feed of daily solar geometry — sunrise, sunset, solar noon, civil/nautical/astronomical twilight, day length, and (via the… |
| 183 | [`switchbot`](./switchbot.md) | SwitchBot | 📦 | `home/` | P2 | S | — | SwitchBot is the popular affordable sensor ecosystem: Hub 2 (with built-in temperature/humidity), Meter Plus, outdoor meters, motion sensors, contact… |
| 184 | [`tiktok`](./tiktok.md) | TikTok | 📦 | `social/` | P2 | S | 🔒 | Short-form video. The headline asset isn't the user's posts — it's the watch history: 10k–50k+ timestamped video views for active users, one of the densest… |
| 185 | [`timery`](./timery.md) | Timery | 📦 | `time-entries/` | P2 | S | — | Timery is a popular Apple-platform frontend (iOS/macOS) for Toggl Track. |
| 186 | [`tinder`](./tinder.md) | Tinder | 📋 | `social/` | P2 | S | 🔒 Needs-David | The largest dating app. Its export is a candid behavioral record — daily swipe counts, every match, and full message threads — that exists nowhere else on… |
| 187 | [`tplink-kasa`](./tplink-kasa.md) | TP-Link Kasa / Tapo | 📦 | `home/` | P2 | S | — | Per-plug energy monitoring from TP-Link's Kasa and Tapo smart plugs: watts right now, daily and monthly kWh per device. |
| 188 | [`trello`](./trello.md) | Trello | 📦 | `tasks/` | P2 | S | — | Kanban-style task/project tool: boards → lists → cards, with due dates, checklists, labels, members, attachments, and comments. |
| 189 | [`tumblr`](./tumblr.md) | Tumblr | 📦 | `social/` | P2 | S | — | Long-form/multimedia blogging platform with a dedicated (if smaller) user base. |
| 190 | [`ulysses`](./ulysses.md) | Ulysses | 📦 | `notes/` | P2 | S | — | Ulysses is a subscription Markdown writing app for Mac/iOS popular with long-form writers, bloggers, and students. |
| 191 | [`venmo`](./venmo.md) | Venmo | 📦 | `finance/` | P2 | S | 🔒 | Venmo peer-payment history — who paid whom, when, how much, and the note attached to each payment — via the official data export. |
| 192 | [`vscode`](./vscode.md) | VS Code | 📦 | `developer/` | P2 | S | — | VS Code's recently-opened workspaces and file-activity state — "what projects and files did I open in VS Code, when" without installing WakaTime. |
| 193 | [`way-of-life`](./way-of-life.md) | Way of Life | 📦 | `habits/` | P2 | S | — | Way of Life is a long-running iOS/Android habit tracker (no Mac app): the user marks each habit yes / no / skip per day and the app surfaces streaks and chains. |
| 194 | [`weatherflow-tempest`](./weatherflow-tempest.md) | WeatherFlow Tempest | 📋 | `home/` | P2 | S | — | Personal weather station (the Tempest unit + Wi-Fi hub). |
| 195 | [`wikipedia`](./wikipedia.md) | Wikipedia Contributions | 📦 | `social/` | P2 | S | — | Edit history for Wikipedia (and any MediaWiki wiki, including Wikidata). |
| 196 | [`actual-budget`](./actual-budget.md) | Actual Budget | 📦 | `finance/` | P2 | M | 🔒 | A local-first, open-source envelope-budgeting app — its users are exactly Trove's audience (privacy-minded, files-on-my-machine people). |
| 197 | [`amazon`](./amazon.md) | Amazon Orders | 📦 | `finance/` | P2 | M | 🔒 | Amazon order history — the most common retail purchase source for most users. |
| 198 | [`apple-app-store`](./apple-app-store.md) | App Store & iTunes Purchases | 📦 | `finance/` | P2 | M | 🔒 | Lifetime purchase history across Apple's storefronts: App Store, iTunes, Apple TV+, Apple Music, Apple Arcade — apps, media, subscriptions, in-app purchases. |
| 199 | [`apple-homekit`](./apple-homekit.md) | Apple HomeKit | 📦 | `home/` | P2 | M | — | HomeKit's local daemon (homed) keeps the user's entire smart-home configuration in a CoreData SQLite database: every accessory, room, zone, scene, and… |
| 200 | [`aranet`](./aranet.md) | Aranet4 | 📦 | `home/` | P2 | M | Needs-sample | The Aranet4 is the gold-standard consumer CO2 monitor — a battery-powered BLE sensor reporting CO2, temperature, humidity, and pressure. |
| 201 | [`audible`](./audible.md) | Audible | 📦 | `media/` | P2 | M | Needs-login | Amazon's audiobook platform — for audiobook listeners it holds the entire library, purchase dates, finish status, and last listening position per title. |
| 202 | [`august-yale`](./august-yale.md) | August / Yale Smart Lock | 📦 | `home/` | P2 | M | Needs-login | Entry/exit log from August and Yale smart locks: every lock/unlock event with who did it and how (app, keypad code, auto-lock). |
| 203 | [`awardwallet`](./awardwallet.md) | AwardWallet | 📦 | `travel/` | P2 | M | 🔒 Needs-login | Loyalty-program aggregator: tracks miles and hotel points across 700+ programs (Marriott Bonvoy, Hilton Honors, IHG, Hyatt, American AAdvantage, United… |
| 204 | [`bandcamp`](./bandcamp.md) | Bandcamp | 📦 | `finance/` | P2 | M | 🔒 Needs-sample | Bandcamp is where people *buy* music — DRM-free albums downloaded and played in local apps. |
| 205 | [`beeper`](./beeper.md) | Beeper | 📋 | `correspondence/` | P2 | M | 🔒 | Paid unified-messaging app bridging WhatsApp, Signal, Telegram, Instagram, Google Messages, LinkedIn, X, Discord, Slack and more into one client — and… |
| 206 | [`bereal`](./bereal.md) | BeReal | 📦 | `photos/` | P2 | M | Needs-sample | Dual-camera daily-photo social app: one front + one back photo per day at a random prompt time. |
| 207 | [`bluesky`](./bluesky.md) | Bluesky | 📦 | `social/` | P2 | M | — | Open-protocol microblogging on AT Protocol. The user's entire repo — posts, likes, follows — is a content-addressed archive that anyone can fetch by DID… |
| 208 | [`box`](./box.md) | Box | 📦 | `files/` | P2 | M | — | Enterprise-leaning cloud storage. Rare on personal Macs (the research doc recommends icebox for the API path), but when the Box Drive desktop app *is*… |
| 209 | [`deezer`](./deezer.md) | Deezer | 📦 | `media/` | P2 | M | — | Music streaming service with a large, EU-centric user base (France especially) — less common on North American Macs but a real Spotify alternative for a… |
| 210 | [`docusign`](./docusign.md) | DocuSign / e-signed documents | 📦 | `files/` | P2 | M | — | DocuSign (and peers — Dropbox Sign, Adobe Sign) is where life's signed paperwork lives: leases, contracts, employment agreements, closings. |
| 211 | [`emporia`](./emporia.md) | Emporia Vue | 📦 | `home/` | P2 | M | Needs-login | Emporia Vue is a panel-mounted home energy monitor that measures usage per circuit via clamp sensors (up to 16 circuits on Vue 2, 18 on the 2026 Vue 3) —… |
| 212 | [`enphase`](./enphase.md) | Enphase Solar | 📦 | `home/` | P2 | M | — | Enphase microinverter solar systems, fronted by the IQ Gateway (Envoy) on the home LAN plus the Enlighten cloud. |
| 213 | [`feedly`](./feedly.md) | Feedly | 📦 | `reading/` | P2 | M | Needs-login | Feedly is the largest cloud RSS service post-Google-Reader. |
| 214 | [`flickr`](./flickr.md) | Flickr | 📦 | `photos/` | P2 | M | — | Long-running photo-hosting service; the audience is photographers with years-deep libraries (often pre-dating smartphone photos), rich tags, album… |
| 215 | [`flight-emails`](./flight-emails.md) | Flight Confirmation Emails (derived) | 📦 | `travel/` | P2 | M | 🔒 | A derived source: rather than connecting to an airline, this is an extractor pass that reads confirmation emails the user already imported and turns them… |
| 216 | [`freestyle-libre`](./freestyle-libre.md) | FreeStyle Libre (LibreView) | 📦 | `health/` | P2 | M | 🔒 | Abbott's FreeStyle Libre is the other major consumer CGM (market share roughly even with Dexcom in some demographics); readings flow sensor → LibreLink app… |
| 217 | [`genetics-variants`](./genetics-variants.md) | Genetic Variant Analysis (derived) | 📦 | `health/` | P2 | M | 🔒 | A derived feature, not a new data source: it reads an already-imported raw genome (23andMe / AncestryDNA 4-column TSV), extracts rsIDs, and cross-references… |
| 218 | [`gog-galaxy`](./gog-galaxy.md) | GOG Galaxy | 📦 | `gaming/` | P2 | M | Needs-sample | GOG's launcher for DRM-free PC games — and, more importantly, a *multi-platform aggregator*: its integration plugins pull Steam, Epic, and other launchers'… |
| 219 | [`google-nest`](./google-nest.md) | Google Nest | 📦 | `home/` | P2 | M | Needs-login | Nest thermostats, cameras, displays, and doorbells via Google's Smart Device Management API. |
| 220 | [`google-photos`](./google-photos.md) | Google Photos | 📦 | `photos/` | P2 | M | 🔒 | Google's cloud photo library — for many users their *primary* photo archive, with a decade-plus of timestamps, GPS points, and face tags. |
| 221 | [`google-pollen`](./google-pollen.md) | Google Pollen | 📦 | `environment/` | P2 | M | Needs-login | Google's pollen-forecast API: daily tree / grass / weed forecasts with species-level breakdown and a UPI (Universal Pollen Index), covering 65+ countries… |
| 222 | [`groupme`](./groupme.md) | GroupMe | 📦 | `correspondence/` | P2 | M | 🔒 Needs-login | Microsoft-owned group-messaging app, popular with US universities, sports teams, and clubs. |
| 223 | [`hacker-news`](./hacker-news.md) | Hacker News | 📦 | `social/` | P2 | M | — | The tech-community forum. For HN regulars, submitted stories and comments are a public intellectual trail; favorites are a curated reading record. |
| 224 | [`honeywell-resideo`](./honeywell-resideo.md) | Honeywell Home (Resideo) | 📋 | `home/` | P2 | M | — | Resideo's Honeywell Home cloud API for the large Honeywell Wi-Fi thermostat installed base — T-Series (T9/T10), Lyric Round, and other Honeywell Home models. |
| 225 | [`inoreader`](./inoreader.md) | Inoreader | 📦 | `reading/` | P2 | M | Needs-login | Inoreader is a power-user cloud RSS service, second to Feedly in user base. |
| 226 | [`instapaper`](./instapaper.md) | Instapaper | 📦 | `reading/` | P2 | M | Needs-sample | The original read-later service, still active with a large long-tenured installed base — many users have a decade-plus of saved articles. |
| 227 | [`iterm2`](./iterm2.md) | iTerm2 | 📋 | `developer/` | P2 | M | 🔒 Needs-sample Needs-David | The dominant third-party macOS terminal. With Shell Integration enabled and "Save copy/paste history and command history to disk" turned on, it keeps… |
| 228 | [`jellyfin`](./jellyfin.md) | Jellyfin | 📦 | `media/` | P2 | M | Needs-login | Free, open-source self-hosted media server — the community alternative to Plex, growing on macOS. |
| 229 | [`lab-pdf-import`](./lab-pdf-import.md) | Lab Results (PDF) | 📦 | `health/` | P2 | M | Needs-sample | The universal non-FHIR fallback for lab results: any lab or hospital portal (MyQuest, patient.labcorp.com, hospital portals, international/specialty labs)… |
| 230 | [`linkding`](./linkding.md) | Linkding | 📦 | `reading/` | P2 | M | Needs-login | Linkding and Shiori are the two popular self-hosted bookmark managers — lightweight, Docker-friendly, open source. |
| 231 | [`literal`](./literal.md) | Literal | 📦 | `reading/` | P2 | M | Needs-login | A smaller, design-forward Goodreads competitor popular with indie-reading circles. |
| 232 | [`lunch-money`](./lunch-money.md) | Lunch Money | 📦 | `finance/` | P2 | M | Needs-login | Lunch Money is a web-first personal-finance app with the best-in-class API among PF apps (the research doc's words). |
| 233 | [`lutron-caseta`](./lutron-caseta.md) | Lutron Caséta | 📦 | `home/` | P2 | M | — | Lutron Caséta is a popular US smart-lighting / shade system. |
| 234 | [`mastodon`](./mastodon.md) | Mastodon | 📦 | `social/` | P2 | M | — | The flagship Fediverse/ActivityPub microblogging network — thousands of independent instances, one open API. |
| 235 | [`matrix`](./matrix.md) | Matrix / Element | 📦 | `correspondence/` | P2 | M | 🔒 | The open, federated messaging protocol; Element is its flagship client. |
| 236 | [`cms-blue-button`](./cms-blue-button.md) | Medicare Blue Button | 📦 | `health/` | P2 | M | 🔒 | CMS Blue Button 2.0 is Medicare's official claims API: a beneficiary authorizes an app with their Medicare.gov login and it returns Part A (inpatient), Part… |
| 237 | [`microsoft-teams`](./microsoft-teams.md) | Microsoft Teams | 📦 | `correspondence/` | P2 | M | 🔒 Needs-login | Microsoft's team-chat + meetings platform, dominant in enterprise. |
| 238 | [`microsoft-todo`](./microsoft-todo.md) | Microsoft To Do | 📦 | `tasks/` | P2 | M | — | Microsoft's task manager (the ex-Wunderlist successor), bundled into the Microsoft 365 / Outlook ecosystem and surfaced in Outlook, Teams, and Windows. |
| 239 | [`moen-flo`](./moen-flo.md) | Moen Flo | 📦 | `home/` | P2 | M | Needs-login | Whole-home smart water monitor (shutoff valve + flow sensor): flow rate, water temperature, pressure, daily/weekly/monthly consumption, leak-detection… |
| 240 | [`monarch-money`](./monarch-money.md) | Monarch Money | 📦 | `finance/` | P2 | M | 🔒 | A popular subscription personal-finance app (the main Mint successor). |
| 241 | [`navidrome`](./navidrome.md) | Navidrome / Subsonic | 📦 | `media/` | P2 | M | — | Self-hosted music servers speaking the Subsonic REST API — Navidrome is the modern flagship; Airsonic-Advanced, Funkwhale, and Ampache speak the same… |
| 242 | [`netnewswire`](./netnewswire.md) | NetNewsWire | 📦 | `reading/` | P2 | M | — | The flagship free, open-source, macOS-native RSS reader, with a large Mac user base. |
| 243 | [`noaa-ndbc`](./noaa-ndbc.md) | NOAA Buoys (NDBC) | 📦 | `environment/` | P2 | M | — | NOAA's National Data Buoy Center — real-time observations from ~1,000 physical buoys and coastal stations across US waters and the Great Lakes: wave height,… |
| 244 | [`noaa-cdo`](./noaa-cdo.md) | NOAA Climate Data Online | 📋 | `environment/` | P2 | M | Needs-David | NOAA's Climate Data Online — daily historical weather observations from actual ground stations (GHCND, the Global Historical Climatology Network): temp… |
| 245 | [`notion`](./notion.md) | Notion | 📦 | `notes/` | P2 | M | — | Notion is a hugely popular block-based workspace: pages, wikis, and databases that double as task managers. |
| 246 | [`notion-airtable`](./notion-airtable.md) | Notion / Airtable Contacts | 📋 | `contacts/` | P2 | M | Needs-David | Many people keep their personal CRM as a hand-rolled Notion database or Airtable base. |
| 247 | [`okcupid`](./okcupid.md) | OkCupid | 📋 | `social/` | P2 | M | 🔒 Needs-sample Needs-David | Dating app (Match Group, like Tinder and Hinge) built around long-form profiles and match questions. |
| 248 | [`omron`](./omron.md) | Omron | 🚫 | `health/` | P2 | M | Needs-login | Omron is the dominant consumer blood-pressure-monitor brand. |
| 249 | [`onenote`](./onenote.md) | OneNote | 📦 | `notes/` | P2 | M | — | OneNote is Microsoft's free-form notebook app, popular in enterprise and education. |
| 250 | [`org-mode`](./org-mode.md) | Org-mode / Plain-text task files | 📦 | `tasks/` | P2 | M | — | Org-mode is Emacs's plain-text outliner/task format — .org files with TODO keywords, priorities, tags, and SCHEDULED/DEADLINE/DONE timestamps. |
| 251 | [`otter`](./otter.md) | Otter.ai | 📦 | `meetings/` | P2 | M | 🔒 | The most widely used meeting notetaker among individuals and academics — which makes the manual-export path worth shipping even though Otter offers no… |
| 252 | [`overcast`](./overcast.md) | Overcast | 📦 | `media/` | P2 | M | — | The leading iOS podcast app for power users — and per the research doc's podcast hierarchy, the best podcast listening history available from any app:… |
| 253 | [`playstation`](./playstation.md) | PlayStation Network | 📦 | `gaming/` | P2 | M | Needs-login | Sony's network for PS4/PS5. Yields the played-titles list with playtime and the full trophy history with unlock timestamps — console gaming that no… |
| 254 | [`plex`](./plex.md) | Plex | 📦 | `media/` | P2 | M | — | Self-hosted media server: movies, TV, and music the user owns, served to their devices. |
| 255 | [`polar`](./polar.md) | Polar | 📦 | `health/` | P2 | M | 🔒 | Polar makes sports watches and heart-rate monitors with a large base among serious endurance athletes. |
| 256 | [`purpleair`](./purpleair.md) | PurpleAir | 📦 | `environment/` | P2 | M | — | PurpleAir is a network of community-run laser particle-counter sensors that report hyperlocal PM2.5. |
| 257 | [`qbserve`](./qbserve.md) | Qbserve | 📦 | `activity/` | P2 | M | Needs-sample | Qbserve is a one-time-purchase macOS time tracker that automatically logs app/website usage and assigns productivity categories, entirely locally (no cloud). |
| 258 | [`reeder`](./reeder.md) | Reeder | 📦 | `reading/` | P2 | M | Needs-sample | Reeder is a popular paid (~$9.99) macOS/iOS RSS reader. |
| 259 | [`roborock`](./roborock.md) | Roborock | 📋 | `home/` | P2 | M | — | Robot-vacuum cleaning history: when each clean ran, how long, area covered, errors, and map snapshots. |
| 260 | [`samsung-health`](./samsung-health.md) | Samsung Health | 📦 | `health/` | P2 | M | 🔒 Needs-sample | Samsung Health is the Android-world counterpart to Apple Health: the aggregation hub for Galaxy Watch wearables, Samsung scales, and HealthKit-style app writes. |
| 261 | [`macos-screenshots`](./macos-screenshots.md) | Screenshots | 🧪 | `photos/` | P2 | M | 🔒 | A passive watcher on the user's saved screenshots: every screenshot already deliberately taken gets its text extracted on-device and made searchable — code… |
| 262 | [`sense-energy`](./sense-energy.md) | Sense Energy Monitor | 📦 | `home/` | P2 | M | Needs-login | Sense is a whole-home energy monitor that clips onto the electrical panel and — its unique trick — disaggregates usage by appliance from electrical… |
| 263 | [`smugmug`](./smugmug.md) | SmugMug | 📋 | `photos/` | P2 | M | Needs-David | Photo hosting/portfolio service used mainly by professional and serious hobbyist photographers. |
| 264 | [`snipd`](./snipd.md) | Snipd | 📦 | `reading/` | P2 | M | — | Snipd is an AI-first podcast player whose signature feature is "snips" — clipped moments from episodes with an AI summary, transcript excerpt, and user notes. |
| 265 | [`swarm`](./swarm.md) | Swarm (Foursquare) | 📦 | `location/` | P2 | M | 🔒 | Swarm is Foursquare's surviving check-in app: a manual "I'm here" log of venues the user visits. |
| 266 | [`tidal`](./tidal.md) | Tidal | 📦 | `media/` | P2 | M | Needs-sample | Hi-fi music streaming service (lossless/Atmos catalog), a meaningful Spotify alternative especially among audiophiles. |
| 267 | [`timing`](./timing.md) | Timing | 📦 | `activity/` | P2 | M | Needs-sample Needs-login | Timing is a subscription macOS automatic time tracker (~$10/mo) popular with freelancers: it observes app, document, and URL usage and rolls it into projects. |
| 268 | [`tripit`](./tripit.md) | TripIt | 📦 | `travel/` | P2 | M | Needs-sample | Trip-itinerary aggregator: users forward booking confirmation emails (or auto-import them) and TripIt assembles trips with flight, hotel, car-rental, and… |
| 269 | [`twitch`](./twitch.md) | Twitch | 📦 | `social/` | P2 | M | Needs-sample | Live-streaming platform. For streamers, the account data and Helix API carry channel/creator history (clips, videos, subscriptions, analytics); for viewers… |
| 270 | [`usgs-water`](./usgs-water.md) | USGS Water Data | 📦 | `environment/` | P2 | M | — | USGS Water Services exposes ~10,000 active US stream gauges with real-time gage height and discharge plus official flood-stage thresholds… |
| 271 | [`green-button`](./green-button.md) | Utility Smart Meter (Green Button) | 📦 | `home/` | P2 | M | Needs-sample | Green Button is the US-standard export of utility smart-meter data: hourly or 15-minute electricity usage (kWh), often gas, occasionally water. |
| 272 | [`wahoo`](./wahoo.md) | Wahoo Fitness | 📦 | `health/` | P2 | M | 🔒 Needs-login | Wahoo makes ELEMNT GPS bike computers and KICKR smart trainers. |
| 273 | [`wallabag`](./wallabag.md) | Wallabag | 📦 | `reading/` | P2 | M | Needs-login | Wallabag is the open-source, self-hostable read-later service (also hosted at wallabag.it for ~11 EUR/yr) — it stores the full article text of everything… |
| 274 | [`webex`](./webex.md) | Webex | 📦 | `meetings/` | P2 | M | 🔒 Needs-login Needs-David | Cisco's enterprise meeting platform. Primarily corporate use, but free personal accounts exist and the transcript API covers both. |
| 275 | [`whatsapp`](./whatsapp.md) | WhatsApp | 📦 | `correspondence/` | P2 | M | 🔒 Needs-sample | The world's largest messenger. On macOS it is effectively a web wrapper: no complete local message DB exists on the Mac — the full database lives on the… |
| 276 | [`windsurf`](./windsurf.md) | Windsurf | 📦 | `developer/` | P2 | M | 🔒 Needs-sample | Windsurf's Cascade chat history: the AI pair-programming conversations from the Codeium-built, VS Code-based editor. |
| 277 | [`xbox`](./xbox.md) | Xbox | 📦 | `gaming/` | P2 | M | Needs-login | Microsoft's gaming network spanning console and PC Game Pass. |
| 278 | [`ynab`](./ynab.md) | YNAB | 📦 | `finance/` | P2 | M | 🔒 | You Need A Budget — the long-running envelope-budgeting app. |
| 279 | [`zed`](./zed.md) | Zed | 📦 | `developer/` | P2 | M | 🔒 Needs-sample | Zed's AI assistant conversation history: the prompts, responses, and thread metadata from the editor's agent panel. |
| 280 | [`arc-timeline`](./arc-timeline.md) | Arc Timeline | 📦 | `location/` | P2 | L | 🔒 Needs-sample | Arc Timeline (bigpaua.com, iOS-only) automatically classifies continuous GPS into visits (with place names — "home", "office") and trips (with detected… |
| 281 | [`clip-embeddings`](./clip-embeddings.md) | CLIP Image Embeddings (derived) | 📋 | `photos/` | P2 | L | Needs-David | Not a data source — a local ML enrichment layer. |
| 282 | [`eight-sleep`](./eight-sleep.md) | Eight Sleep | 📦 | `health/` | P2 | L | Needs-login | Smart-mattress cover ("Pod") that records sleep biometrics every ~2 seconds: beat-by-beat HR, HRV, breath rate, toss/turn events, bed/room temperature. |
| 283 | [`gocardless`](./gocardless.md) | GoCardless Bank Account Data (EU/UK) | 📋 | `finance/` | P2 | L | 🔒 Needs-David | The EU/UK answer to SimpleFIN. SimpleFIN is US-centric, which leaves European users of Trove without live bank sync — a real gap for the "built for anyone"… |
| 284 | [`icloud-contacts`](./icloud-contacts.md) | iCloud Contacts (CardDAV) | 📦 | `contacts/` | P2 | L | Needs-login | iCloud's contact store over the standard CardDAV protocol. |
| 285 | [`omnifocus`](./omnifocus.md) | OmniFocus | 📦 | `tasks/` | P2 | L | Needs-sample | OmniFocus is a heavyweight Mac/iOS GTD task manager favored by power users. |
| 286 | [`renpho`](./renpho.md) | Renpho | 📋 | `health/` | P2 | L | Needs-sample Needs-David | Renpho makes inexpensive Bluetooth smart scales with a companion app that records weight plus bioimpedance body composition (fat %, muscle mass, bone mass,… |
| 287 | [`suunto`](./suunto.md) | Suunto | 📦 | `health/` | P2 | L | 🔒 | GPS sports watches (diving, trail, multisport heritage). |
| 288 | [`tesla-energy`](./tesla-energy.md) | Tesla Powerwall + Solar | 📦 | `home/` | P2 | L | Needs-login | Solar production and Powerwall battery storage telemetry for Tesla Energy owners: generation, home consumption, grid import/export, battery… |
| 289 | [`ultrahuman`](./ultrahuman.md) | Ultrahuman | 📦 | `health/` | P2 | L | 🔒 Needs-login | Ultrahuman Ring AIR — smart ring in the Oura segment, growing base. |
| 290 | [`amazfit`](./amazfit.md) | Amazfit (Zepp) | 📋 | `health/` | P2 | XL | 🔒 Needs-David | Amazfit smartwatches (Zepp Health, Xiaomi ecosystem) — large budget-watch user base. |

## Deferred — read-time features (post-wave, not built by the loop)

These are **derived layers, not collectors** — they read what the wave
collects rather than pulling a new source, so per `docs/integration-pipeline.md`
they belong to read-time organization *after* the wave. Catalogued here for
visibility; the loop skips them (no order number). Revisit post-wave.

| Provider | Name | Domain | Note |
|---|---|---|---|
| [`interaction-graph`](./interaction-graph.md) | Interaction Graph (derived) | `contacts/` | derived read-time layer; depends on collected sources |
| [`entity-resolution`](./entity-resolution.md) | Entity Resolution (cross-source identity merge) | `contacts/` | derived read-time layer; depends on collected sources |

| — | [`apple-books`](./apple-books.md) | Apple Books | 🧪 | `media/` | P2 | S | — | Apple's built-in ebook reader on macOS/iOS. Its local SQLite databases hold the user's library, per-book reading progress and finish dates, and every… |
| — | [`apple-health`](./apple-health.md) | Apple Health | 🧪 | `health/` | P0 | M | 🔒 | The platform health hub: iPhone Health app → Export All Health Data → export.zip, the single richest health artifact most users can produce. |
| — | [`apple-podcasts`](./apple-podcasts.md) | Apple Podcasts | 🧪 | `media/` | P2 | M | — | Apple's built-in podcast app — the default for Mac/iPhone users who never chose a player, so it covers the broadest slice of podcast listeners with zero… |
| — | [`bank-statements`](./bank-statements.md) | Bank Statement Import (CSV/OFX/QFX) | 🧪 | `finance/` | P0 | S | 🔒 | File import for bank and card statements downloaded from any bank portal. |
| — | [`calls`](./calls.md) | Calls & FaceTime | 🧪 | `correspondence/` | P0 | S | — | The Mac's local call-history database: iPhone calls (synced via Continuity), FaceTime video and audio, and Mac cellular-relay calls. |
| — | [`copilot-money`](./copilot-money.md) | Copilot Money | 🧪 | `finance/` | P0 | S | 🔒 | Copilot Money is a popular iOS/Mac personal-finance app. |
| — | [`email`](./email.md) | Email Import (.mbox / .eml) | 🧪 | `correspondence/` | P0 | S | 🔒 | The universal email backstop: any RFC 4155-ish .mbox export — Google Takeout (one .mbox per label), Apple Mail File → Export Mailbox, Thunderbird,… |
| — | [`fantastical`](./fantastical.md) | Fantastical | 🧪 | `calendar/` | P2 | S | — | Fantastical (Flexibits) is a popular Mac/iOS calendar-and-tasks client. |
| — | [`google-gmail`](./google-gmail.md) | Gmail | 🧪 | `correspondence/` | P0 | M | 🔒 | The world's largest consumer email service. Email is the densest single correspondence source most users have — decades of conversations, receipts,… |
| — | [`google-calendar`](./google-calendar.md) | Google Calendar | 🧪 | `calendar/` | P1 | S | — | Google's calendar service — one of the two most common personal calendars (with Apple Calendar). |
| — | [`google-contacts`](./google-contacts.md) | Google Contacts | 🧪 | `contacts/` | P1 | M | — | Google's address book — for many users the primary contact store, and it often does *not* sync to macOS Contacts unless the Google account is added in… |
| — | [`google-tasks`](./google-tasks.md) | Google Tasks | 🧪 | `tasks/` | P2 | S | — | Google's lightweight to-do list, surfaced inside Gmail and Google Calendar. |
| — | [`imessage`](./imessage.md) | iMessage / SMS | 🧪 | `correspondence/` | P0 | S | 🔒 | Apple Messages — iMessage, SMS, and RCS (iOS 18+) — read straight from the local ~/Library/Messages/chat.db. |
| — | [`letterboxd`](./letterboxd.md) | Letterboxd | 🧪 | `media/` | P1 | S | — | The dominant film-diary app: users log every film they watch with a date, star rating, rewatch flag, and review. |
| — | [`loseit`](./loseit.md) | Lose It! | 🧪 | `health/` | P2 | S | — | Calorie/nutrition tracker with a crowdsourced food database. |
| — | [`open-meteo`](./open-meteo.md) | Open-Meteo | 🧪 | `environment/` | P1 | M | — | Free, keyless weather and environmental forecast API (Copernicus CAMS / ERA5 under the hood). |
| — | [`oura`](./oura.md) | Oura Ring | 🧪 | `health/` | P0 | S | — | Smart-ring health tracker: sleep, readiness, activity, continuous heart rate, SpO2, stress, and a stack of Oura-computed composite scores. |
| — | [`safari`](./safari.md) | Safari | 🧪 | `browser/` | P2 | S | — | Apple's default Mac browser, read entirely from its local container. |
| — | [`simplefin`](./simplefin.md) | SimpleFIN (Bank Sync) | 🧪 | `finance/` | P0 | S | 🔒 | SimpleFIN Bridge is a bank/card aggregator (~16k institutions via the MX network) where each user owns their credential (~$1.50/mo) — the only aggregator… |
| — | [`slack`](./slack.md) | Slack | 🧪 | `correspondence/` | P1 | M | 🔒 | The dominant team-chat platform; for many users the bulk of their work conversations live nowhere else. |
| — | [`google-youtube`](./google-youtube.md) | YouTube | 🧪 | `media/` | P1 | M | — | The user's video life: subscriptions, playlists, liked videos, and — the crown jewel — complete watch history back to account creation. |
| — | [`zelle`](./zelle.md) | Zelle | 🧪 | `finance/` | P2 | S | 🔒 | Zelle is the bank-embedded US peer-to-peer payment network. |
| — | [`500px`](./500px.md) | 500px | 🚫 | `photos/` | P2 | XL | — | Photo-sharing/portfolio community for photographers, acquired by Visual China Group; significantly declined in Western markets since. |
| — | [`apple-journal`](./apple-journal.md) | Apple Journal | 🚫 | `notes/` | P2 | M | — | Apple Journal is Apple's first-party journaling app — iOS/iPadOS-only since December 2023, coming to the Mac with macOS Tahoe 26 (fall 2026). |
| — | [`apple-maps`](./apple-maps.md) | Apple Maps Visited Places | 🚫 | `location/` | P2 | XL | 🔒 | iOS 26's opt-in "Visited Places" log in the Maps app (Profile → Places → Visited Places): a private, on-device record of places the user has been. |
| — | [`apple-significant-locations`](./apple-significant-locations.md) | Apple Significant Locations | 🚫 | `location/` | P2 | XL | 🔒 | macOS/iOS "Significant Locations": the OS's private log of frequently visited places, encrypted with Secure-Enclave keys — unreadable by any other process even with Full Disk Access. |
| — | [`blitzortung`](./blitzortung.md) | Blitzortung Lightning | 🚫 | `environment/` | P2 | L | — | Community volunteer network of ~3,000 lightning-detection sensors globally, producing excellent open lightning-strike data. |
| — | [`dental-records`](./dental-records.md) | Dental Records | 🚫 | `health/` | P2 | S | 🔒 | The dental slice of personal medical history — treatment notes, perio charts, X-rays. |
| — | [`disney-hulu-max`](./disney-hulu-max.md) | Disney+ / Hulu / Max | 🚫 | `media/` | P2 | M | — | The big three subscription video streamers (Disney+, Hulu, HBO Max/Max), grouped because they share the same non-situation: large watch histories locked… |
| — | [`ecobee`](./ecobee.md) | Ecobee | 🚫 | `home/` | P2 | L | — | Ecobee makes smart thermostats with rich runtime, setpoint, and occupancy- sensor history — premium features (energy reports, runtime history) are… |
| — | [`epic-games`](./epic-games.md) | Epic Games Store | 🚫 | `gaming/` | P2 | M | — | PC game library and playtime from the Epic Games Store launcher. |
| — | [`macos-fsevents`](./macos-fsevents.md) | FSEvents Journal | 🚫 | `files/` | P2 | L | — | The on-disk FSEvents journal each APFS volume keeps at /.fseventsd/ — a binary log of every file create/modify/delete/rename on the volume. |
| — | [`google-analytics`](./google-analytics.md) | Google Analytics (GA4) | 🚫 | `social/` | P2 | M | — | Google's website-analytics product. The GA4 Data API returns the site *owner's* aggregate visitor statistics — sessions, pageviews, traffic sources, countries. |
| — | [`grocery-loyalty`](./grocery-loyalty.md) | Grocery & Retail Loyalty Programs | 🚫 | `finance/` | P2 | XL | 🔒 | The itemized record of what you actually bought at the grocery store — arguably the most detailed spending data that exists about a person (Kroger earned… |
| — | [`kagi`](./kagi.md) | Kagi | 🚫 | `browser/` | P2 | XL | — | Kagi is a paid, privacy-first search engine. Its privacy model intentionally prevents any server-side search history from existing: no export, no history… |
| — | [`life360`](./life360.md) | Life360 | 🚫 | `location/` | P2 | L | 🔒 | Life360 is a family location-sharing app: circles of family members sharing continuous precise location. |
| — | [`line`](./line.md) | LINE | 🚫 | `correspondence/` | P2 | L | 🔒 | The dominant messenger of Japan, Thailand, and Taiwan (~200M users). |
| — | [`macos-barometer`](./macos-barometer.md) | Mac Barometer | 🚫 | `environment/` | P2 | XL | — | Apple-Silicon Macs contain barometer hardware, but macOS provides no public API to read it. |
| — | [`macos-recent-files`](./macos-recent-files.md) | macOS Recent Files | 🚫 | `files/` | P2 | L | — | The "Recent Documents" lists macOS keeps per app and globally (com.apple.sharedfilelist SFL2 files) — in principle a record of which files the user opened… |
| — | [`matter`](./matter.md) | Matter | 🚫 | `reading/` | P2 | M | — | Matter is a polished iOS-first read-later app with a sizable user base, which grew further after Pocket's 2025 shutdown. |
| — | [`nintendo-switch`](./nintendo-switch.md) | Nintendo Switch | 🚫 | `gaming/` | P2 | L | — | Console gaming playtime from Nintendo's Switch ecosystem. |
| — | [`noom`](./noom.md) | Noom | 🚫 | `health/` | P2 | S | — | Weight-loss / behavioral-coaching app with meal logging. |
| — | [`pandora`](./pandora.md) | Pandora | 🚫 | `media/` | P2 | L | — | Radio-style music streaming (stations seeded from artists/songs, tuned by thumbs). |
| — | [`plaid`](./plaid.md) | Plaid | 🚫 | `finance/` | P2 | XL | 🔒 | The dominant US bank-data aggregator — broadest institution coverage, near-real-time transactions, and an Investments product covering brokerage accounts. |
| — | [`plausible`](./plausible.md) | Plausible Analytics | 🚫 | `social/` | P2 | S | — | Privacy-respecting website analytics (cloud or self-hosted; Community Edition v2.2 is fully open-source AGPL-3.0). |
| — | [`snaptrade`](./snaptrade.md) | SnapTrade (Brokerage Aggregator) | 🚫 | `finance/` | P2 | L | 🔒 | A brokerage-data aggregator: 30+ brokerages (Robinhood, Schwab, Fidelity, TD, IBKR, …) behind one REST API, with an official Rust SDK and a generous free… |
| — | [`soundcloud`](./soundcloud.md) | SoundCloud | 🚫 | `media/` | P2 | L | — | Streaming platform centered on independent and user-uploaded music. |
| — | [`streaks`](./streaks.md) | Streaks | 🚫 | `habits/` | P2 | M | — | Popular iOS-primary habit tracker (Streaks by Crunchy Bagel, App Store id 963034692) with a Catalyst Mac build. |
| — | [`sunsama`](./sunsama.md) | Sunsama | 🚫 | `tasks/` | P2 | L | — | Sunsama is a daily-planning app that aggregates tasks from other tools (Todoist, Asana, Linear, Jira, calendars) into a guided daily/weekly plan. |
| — | [`teller`](./teller.md) | Teller.io | 🚫 | `finance/` | P2 | XL | 🔒 | A bank-aggregation backend: direct API connections to banks (no screen scraping), near-real-time transactions and balances. |
| — | [`transit-cards`](./transit-cards.md) | Transit Cards (Clipper, Oyster, ORCA, …) | 🚫 | `travel/` | P2 | L | 🔒 | Stored-value transit fare cards (Clipper in the Bay Area, Oyster in London, ORCA in Seattle, Ventra in Chicago, CharlieCard in Boston, …). |
| — | [`tv-time`](./tv-time.md) | TV Time | 🚫 | `media/` | P2 | M | — | TV episode tracker (which series, which episode, when watched, watch counts). |
| — | [`wechat`](./wechat.md) | WeChat | 🚫 | `correspondence/` | P2 | XL | 🔒 | Tencent's messaging super-app (~800M users, primarily East Asia). |
