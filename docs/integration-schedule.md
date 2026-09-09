# Integration schedule

<!-- GENERATED — do not edit by hand. -->

Generated from `INTEGRATIONS` (each integration's `Behavior` in its `IntegrationDef`).
The `schedule_doc` test pins this file to the registry byte-for-byte; after
changing a def, regenerate with:

```bash
TROVE_REGEN=1 cargo test -p trove-core --test schedule_doc
```

| Id | Name | Shape | Schedule |
|---|---|---|---|
| `activity` | App activity | Live | always on; ticked every poll (5 s) |
| `music-scrobbler` | Apple Music scrobbler | Live | always on; ticked every poll (5 s) |
| `browser-extension` | Browser extension | Native host | event-driven; the browser extension's native-messaging host writes as events arrive |
| `browser-ads` | Ad observation | Native host | event-driven; the browser extension's native-messaging host writes as events arrive |
| `browser-ads-identify` | Advertiser identity lookup | Native host | event-driven; the browser extension's native-messaging host writes as events arrive |
| `chrome-history` | Chrome history | Periodic | every 15 min |
| `safari-history` | Safari history | Covered by `chrome-history` | runs with `chrome-history`'s pass |
| `imessage` | Messages (iMessage) | Periodic | every 15 min |
| `calls` | Calls & FaceTime | Periodic | every 15 min |
| `music-library` | Apple Music library | Periodic | checked every 15 min, runs once per local day |
| `podcasts` | Apple Podcasts | Periodic | checked every 15 min, runs only when the source database changed; retries on failure without burning the gate |
| `books` | Apple Books | Periodic | checked every 15 min, runs once per local day |
| `screen-time` | Screen Time (other devices) | Periodic | checked every 15 min, runs only when the source database changed; retries on failure without burning the gate |
| `nowplaying` | Now Playing (iPhone & iPad) | Covered by `screen-time` | runs with `screen-time`'s pass |
| `screen-time-this-mac` | Screen Time (this Mac, backup) | Covered by `screen-time` | runs with `screen-time`'s pass |
| `calendar` | Calendar | Periodic | every 15 min |
| `apple-reminders` | Apple Reminders | Periodic | every 15 min |
| `ticktick` | TickTick | Periodic | every 15 min |
| `bank-sync` | Banks & cards (SimpleFIN) | Periodic | checked every 15 min, runs once per local day |
| `csv-import` | Bank statement import (CSV) | Import | manual; runs when you import a file |
| `oura` | Oura Ring | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-gmail` | Gmail | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-calendar` | Google Calendar | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-contacts` | Google Contacts | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-tasks` | Google Tasks | Periodic | every 15 min |
| `google-youtube` | YouTube | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-books` | Google Play Books | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `weather` | Weather | Periodic | every 15 min |
| `health` | Apple Health | Import | manual; runs when you import a file |
| `email` | Email archives | Import | manual; runs when you import a file |
| `slack` | Slack exports | Import | manual; runs when you import a file |
| `letterboxd` | Letterboxd | Import | manual; runs when you import a file |
| `apple-significant-locations` | Apple Significant Locations | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `23andme` | 23andMe | Import | manual; runs when you import a file |
| `amazfit` | Amazfit (Zepp) | Not wired | nothing runs yet |
| `ancestrydna` | AncestryDNA | Import | manual; runs when you import a file |
| `bearable` | Bearable | Import | manual; runs when you import a file |
| `cms-blue-button` | Medicare Blue Button | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `coros` | COROS | Import | manual; runs when you import a file |
| `cronometer` | Cronometer | Import | manual; runs when you import a file |
| `dental-records` | Dental Records | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `dexcom` | Dexcom | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `eight-sleep` | Eight Sleep | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `epic-mychart` | Epic MyChart | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `fitbit` | Fitbit | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `freestyle-libre` | FreeStyle Libre (LibreView) | Import | manual; runs when you import a file |
| `garmin` | Garmin Connect | Import | manual; runs when you import a file |
| `genetics-variants` | Genetic Variant Analysis (derived) | Import | manual; runs when you import a file |
| `lab-pdf-import` | Lab Results (PDF) | Import | manual; runs when you import a file |
| `labcorp` | Labcorp | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `levels-health` | Levels | Import | manual; runs when you import a file |
| `lifesum` | Lifesum | Import | manual; runs when you import a file |
| `macrofactor` | MacroFactor | Import | manual; runs when you import a file |
| `medisafe` | Medisafe | Import | manual; runs when you import a file |
| `myfitnesspal` | MyFitnessPal | Import | manual; runs when you import a file |
| `nightscout` | Nightscout | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `noom` | Noom | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `omron` | Omron | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `pharmacy-prescriptions` | Pharmacy Prescriptions | Import | manual; runs when you import a file |
| `polar` | Polar | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `quest-diagnostics` | Quest Diagnostics | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `renpho` | Renpho | Not wired | nothing runs yet |
| `samsung-health` | Samsung Health | Import | manual; runs when you import a file |
| `smart-on-fhir` | Medical Records (SMART on FHIR) | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `strava` | Strava | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `suunto` | Suunto | Import | manual; runs when you import a file |
| `ultrahuman` | Ultrahuman | Periodic | every 4 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `wahoo` | Wahoo Fitness | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `whoop` | WHOOP | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `withings` | Withings | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `500px` | 500px | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `apple-photos` | Apple Photos | Periodic | checked every 4 hours, runs only when the source database changed; retries on failure without burning the gate |
| `bereal` | BeReal | Import | manual; runs when you import a file |
| `clip-embeddings` | CLIP Image Embeddings (derived) | Not wired | nothing runs yet |
| `exif-import` | Image Files (EXIF) | Import | manual; runs when you import a file |
| `flickr` | Flickr | Import | manual; runs when you import a file |
| `google-photos` | Google Photos | Import | manual; runs when you import a file |
| `macos-screenshots` | Screenshots | Live | always on; ticked every poll (5 s) |
| `smugmug` | SmugMug | Not wired | nothing runs yet |
| `actual-budget` | Actual Budget | Periodic | hourly |
| `amazon` | Amazon Orders | Import | manual; runs when you import a file |
| `apple-app-store` | App Store & iTunes Purchases | Import | manual; runs when you import a file |
| `apple-card` | Apple Card, Cash & Savings | Import | manual; runs when you import a file |
| `bandcamp` | Bandcamp | Import | manual; runs when you import a file |
| `bitcoin` | Bitcoin Wallet (Blockstream) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `cash-app` | Cash App | Import | manual; runs when you import a file |
| `coinbase` | Coinbase | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `crypto-tax-exports` | Koinly / CoinTracker Exports | Import | manual; runs when you import a file |
| `ethereum` | Ethereum Wallet (Etherscan) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `fidelity` | Fidelity | Import | manual; runs when you import a file |
| `gocardless` | GoCardless Bank Account Data (EU/UK) | Not wired | nothing runs yet |
| `grocery-loyalty` | Grocery & Retail Loyalty Programs | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `interactive-brokers` | Interactive Brokers | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `kraken` | Kraken | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `lunch-money` | Lunch Money | Periodic | every 6 hours |
| `monarch-money` | Monarch Money | Import | manual; runs when you import a file |
| `paypal` | PayPal | Import | manual; runs when you import a file |
| `plaid` | Plaid | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `robinhood` | Robinhood | Not wired | nothing runs yet |
| `schwab` | Charles Schwab | Periodic | every 4 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `snaptrade` | SnapTrade | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `stripe` | Stripe Billing | Not wired | nothing runs yet |
| `teller` | Teller.io | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `vanguard` | Vanguard | Import | manual; runs when you import a file |
| `venmo` | Venmo | Import | manual; runs when you import a file |
| `ynab` | YNAB | Periodic | every 4 hours |
| `airbnb` | Airbnb | Import | manual; runs when you import a file |
| `awardwallet` | AwardWallet | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `flight-emails` | Flight Confirmation Emails | Import | manual; runs when you import a file |
| `flighty` | Flighty | Periodic | checked every 24 hours, runs once per local day |
| `myflightradar24` | myFlightRadar24 | Import | manual; runs when you import a file |
| `transit-cards` | Transit Cards (Clipper, Oyster, ORCA, …) | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `tripit` | TripIt | Import | manual; runs when you import a file |
| `airnow` | AirNow (EPA AQI) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `blitzortung` | Blitzortung Lightning | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `google-pollen` | Google Pollen | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `macos-barometer` | Mac Barometer | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `macos-microphone` | Mac Microphone Ambient Sound | Not wired | nothing runs yet |
| `nasa-firms` | NASA FIRMS Wildfire | Periodic | checked every 24 hours, runs once per local day |
| `noaa-cdo` | NOAA Climate Data Online | Not wired | nothing runs yet |
| `noaa-co-ops` | NOAA Tides & Currents | Periodic | checked every 24 hours, runs once per local day |
| `noaa-ndbc` | NOAA Buoys (NDBC) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `noaa-swpc` | NOAA Space Weather | Periodic | every 3 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `nws` | National Weather Service | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `purpleair` | PurpleAir | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `sunrise-sunset` | Sunrise-Sunset.org | Periodic | checked every 6 hours, runs once per local day |
| `usgs-earthquakes` | USGS Earthquakes | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `usgs-water` | USGS Water Data | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `usno` | USNO Astronomy | Periodic | checked every 6 hours, runs once per local day |
| `waqi` | World Air Quality Index | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `airthings` | Airthings | Not wired | nothing runs yet |
| `amazon-alexa` | Amazon Alexa | Import | manual; runs when you import a file |
| `ambient-weather` | Ambient Weather | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `apple-homekit` | Apple HomeKit | Periodic | checked every 24 hours, runs only when the source database changed; retries on failure without burning the gate |
| `aranet` | Aranet4 | Import | manual; runs when you import a file |
| `august-yale` | August / Yale Smart Lock | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `awair` | Awair | Not wired | nothing runs yet |
| `ecobee` | Ecobee | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `emporia` | Emporia Vue | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `enphase` | Enphase Solar | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-nest` | Google Nest | Periodic | every 5 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `green-button` | Utility Smart Meter (Green Button) | Import | manual; runs when you import a file |
| `home-assistant` | Home Assistant | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `honeywell-resideo` | Honeywell Home (Resideo) | Not wired | nothing runs yet |
| `lutron-caseta` | Lutron Caséta | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `moen-flo` | Moen Flo | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `netatmo` | Netatmo Weather Station | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `philips-hue` | Philips Hue | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `ring` | Ring | Periodic | every 12 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `roborock` | Roborock | Not wired | nothing runs yet |
| `sense-energy` | Sense Energy Monitor | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `switchbot` | SwitchBot | Periodic | every 5 min |
| `tesla-energy` | Tesla Powerwall + Solar | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `tplink-kasa` | TP-Link Kasa / Tapo | Periodic | every 5 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `weatherflow-tempest` | WeatherFlow Tempest | Not wired | nothing runs yet |
| `alfred` | Alfred Clipboard | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `bitbucket` | Bitbucket | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `chatgpt` | ChatGPT | Import | manual; runs when you import a file |
| `claude-code` | Claude Code | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `claude-code-transcripts` | Claude Code — full transcripts | Covered by `claude-code` | runs with `claude-code`'s pass |
| `cursor` | Cursor | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `cursor-transcripts` | Cursor — full transcripts | Covered by `cursor` | runs with `cursor`'s pass |
| `github` | GitHub | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `github-copilot` | GitHub Copilot | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `github-copilot-transcripts` | GitHub Copilot — full transcripts | Covered by `github-copilot` | runs with `github-copilot`'s pass |
| `gitlab` | GitLab | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `iterm2` | iTerm2 | Not wired | nothing runs yet |
| `jetbrains` | JetBrains IDEs | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `local-git` | Local Git Activity | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `shell-history` | Shell History | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `vscode` | VS Code | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `windsurf` | Windsurf | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `zed` | Zed | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `zed-transcripts` | Zed — full transcripts | Covered by `zed` | runs with `zed`'s pass |
| `amazing-marvin` | Amazing Marvin | Not wired | nothing runs yet |
| `asana` | Asana | Periodic | every 15 min |
| `jira` | Jira | Periodic | every 15 min |
| `linear` | Linear | Periodic | every 15 min |
| `microsoft-todo` | Microsoft To Do | Periodic | every 15 min |
| `motion` | Motion | Not wired | nothing runs yet |
| `omnifocus` | OmniFocus | Periodic | hourly |
| `org-mode` | Org-mode / Plain-text Tasks | Periodic | every 15 min |
| `sunsama` | Sunsama | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `things` | Things 3 | Periodic | every 15 min |
| `todoist` | Todoist | Periodic | every 15 min |
| `trello` | Trello | Periodic | every 15 min |
| `apple-contacts` | Apple Contacts | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `clay` | Clay (Mesh) | Import | manual; runs when you import a file |
| `dex` | Dex (Personal CRM) | Not wired | nothing runs yet |
| `icloud-contacts` | iCloud Contacts (CardDAV) | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `linkedin` | LinkedIn | Import | manual; runs when you import a file |
| `monica` | Monica (Personal CRM) | Not wired | nothing runs yet |
| `notion-airtable` | Notion / Airtable Contacts | Not wired | nothing runs yet |
| `vcard` | vCard Import (.vcf) | Import | manual; runs when you import a file |
| `apple-journal` | Apple Journal | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `apple-notes` | Apple Notes | Periodic | every 6 hours |
| `bear` | Bear | Periodic | hourly |
| `capacities` | Capacities | Import | manual; runs when you import a file |
| `craft` | Craft | Not wired | nothing runs yet |
| `day-one` | Day One | Import | manual; runs when you import a file |
| `drafts` | Drafts | Periodic | hourly |
| `evernote` | Evernote | Import | manual; runs when you import a file |
| `google-keep` | Google Keep | Import | manual; runs when you import a file |
| `logseq` | Logseq | Import | manual; runs when you import a file |
| `notion` | Notion | Periodic | every 30 min |
| `obsidian` | Obsidian | Periodic | hourly |
| `onenote` | OneNote | Periodic | hourly |
| `reflect` | Reflect | Import | manual; runs when you import a file |
| `roam` | Roam Research | Import | manual; runs when you import a file |
| `simplenote` | Simplenote | Import | manual; runs when you import a file |
| `standard-notes` | Standard Notes | Import | manual; runs when you import a file |
| `stoic` | Stoic | Import | manual; runs when you import a file |
| `ulysses` | Ulysses | Import | manual; runs when you import a file |
| `apple-mail` | Apple Mail | Periodic | every 15 min |
| `beeper` | Beeper | Not wired | nothing runs yet |
| `discord` | Discord | Import | manual; runs when you import a file |
| `facebook-messenger` | Facebook Messenger | Import | manual; runs when you import a file |
| `fastmail` | Fastmail | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-chat` | Google Chat | Import | manual; runs when you import a file |
| `google-voice` | Google Voice | Import | manual; runs when you import a file |
| `groupme` | GroupMe | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `imap` | IMAP Email (any provider) | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `irc` | IRC Logs (ZNC / WeeChat / Irssi) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `line` | LINE | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `matrix` | Matrix / Element | Periodic | every 15 min |
| `microsoft-teams` | Microsoft Teams | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `outlook` | Microsoft Outlook | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `protonmail` | ProtonMail | Import | manual; runs when you import a file |
| `signal` | Signal | Periodic | every 15 min |
| `skype` | Skype (archival) | Import | manual; runs when you import a file |
| `snapchat` | Snapchat | Import | manual; runs when you import a file |
| `telegram` | Telegram | Import | manual; runs when you import a file |
| `wechat` | WeChat | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `whatsapp` | WhatsApp | Import | manual; runs when you import a file |
| `apple-maps` | Apple Maps Visited Places | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `arc-timeline` | Arc Timeline | Import | manual; runs when you import a file |
| `google-maps` | Google Maps Saved Places | Import | manual; runs when you import a file |
| `google-timeline` | Google Timeline | Import | manual; runs when you import a file |
| `life360` | Life360 | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `overland` | Overland (iOS GPS Logger) | Not wired | nothing runs yet |
| `owntracks` | OwnTracks | Live | always on; ticked every poll (5 s) |
| `smartcar` | Smartcar | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `swarm` | Swarm (Foursquare) | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `tesla` | Tesla | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `apple-voice-memos` | Apple Voice Memos | Periodic | hourly |
| `apple-voicemail` | Visual Voicemail (iPhone backup) | Periodic | hourly |
| `audible` | Audible | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `deezer` | Deezer | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `disney-hulu-max` | Disney+ / Hulu / Max | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `goodreads` | Goodreads | Import | manual; runs when you import a file |
| `hardcover` | Hardcover | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `imdb` | IMDb | Import | manual; runs when you import a file |
| `jellyfin` | Jellyfin | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `lastfm` | Last.fm | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `libby` | Libby / OverDrive | Import | manual; runs when you import a file |
| `listenbrainz` | ListenBrainz | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `literal` | Literal | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `mediaremote` | Mac Now Playing (MediaRemote) | Not wired | nothing runs yet |
| `navidrome` | Navidrome / Subsonic | Periodic | every 5 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `netflix` | Netflix | Import | manual; runs when you import a file |
| `overcast` | Overcast | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `pandora` | Pandora | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `plex` | Plex | Periodic | checked hourly, runs only when the source database changed; retries on failure without burning the gate |
| `pocket-casts` | Pocket Casts | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `prime-video` | Prime Video | Import | manual; runs when you import a file |
| `shazam` | Shazam | Periodic | every 15 min |
| `simkl` | Simkl | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `soundcloud` | SoundCloud | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `spotify` | Spotify | Not wired | nothing runs yet |
| `storygraph` | StoryGraph | Import | manual; runs when you import a file |
| `tidal` | Tidal | Import | manual; runs when you import a file |
| `trakt` | Trakt | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `tv-time` | TV Time | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `youtube-music` | YouTube Music | Import | manual; runs when you import a file |
| `bluesky` | Bluesky | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `bumble` | Bumble | Not wired | nothing runs yet |
| `facebook` | Facebook | Import | manual; runs when you import a file |
| `google-analytics` | Google Analytics (GA4) | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `hacker-news` | Hacker News | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `hinge` | Hinge | Not wired | nothing runs yet |
| `instagram` | Instagram | Import | manual; runs when you import a file |
| `mastodon` | Mastodon | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `mastodon-archive` | Mastodon Archive | Import | manual; runs when you import a file |
| `okcupid` | OkCupid | Not wired | nothing runs yet |
| `pinterest` | Pinterest | Import | manual; runs when you import a file |
| `plausible` | Plausible Analytics | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `reddit` | Reddit | Import | manual; runs when you import a file |
| `substack` | Substack | Not wired | nothing runs yet |
| `threads` | Threads | Import | manual; runs when you import a file |
| `tiktok` | TikTok | Import | manual; runs when you import a file |
| `tinder` | Tinder | Not wired | nothing runs yet |
| `tumblr` | Tumblr | Import | manual; runs when you import a file |
| `twitch` | Twitch | Import | manual; runs when you import a file |
| `wikipedia` | Wikipedia Contributions | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `x-twitter` | X (Twitter) | Import | manual; runs when you import a file |
| `boardgamegeek` | BoardGameGeek | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `chess-com` | Chess.com | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `epic-games` | Epic Games Store | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `gog-galaxy` | GOG Galaxy | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `lichess` | Lichess | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `nintendo-switch` | Nintendo Switch | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `playstation` | PlayStation Network | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `retroachievements` | RetroAchievements | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `steam` | Steam | Periodic | every 8 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `xbox` | Xbox | Periodic | every 6 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `box` | Box | Periodic | hourly |
| `docusign` | Signed Documents (DocuSign, Dropbox Sign, Adobe Sign) | Import | manual; runs when you import a file |
| `dropbox` | Dropbox | Periodic | every 15 min |
| `google-drive` | Google Drive | Periodic | every 15 min |
| `icloud-drive` | iCloud Drive | Periodic | every 15 min |
| `macos-downloads` | macOS Downloads | Periodic | hourly |
| `macos-fsevents` | FSEvents Journal | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `macos-recent-files` | macOS Recent Files | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `onedrive` | OneDrive | Periodic | every 15 min |
| `password-manager` | Password Manager Metadata (1Password, Bitwarden) | Not wired | nothing runs yet |
| `cal-com` | Cal.com | Periodic | every 30 min |
| `caldav` | CalDAV (any server) | Periodic | every 15 min |
| `calendly` | Calendly | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `outlook-calendar` | Outlook Calendar | Periodic | every 15 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `clockify` | Clockify | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `harvest` | Harvest | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `timery` | Timery | Covered by `toggl-track` | runs with `toggl-track`'s pass |
| `toggl-track` | Toggl Track | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `fathom` | Fathom | Periodic | every 15 min |
| `fireflies` | Fireflies.ai | Periodic | every 4 hours |
| `google-meet` | Google Meet | Periodic | every 15 min |
| `granola` | Granola | Periodic | every 30 min |
| `krisp` | Krisp | Import | manual; runs when you import a file |
| `otter` | Otter.ai | Import | manual; runs when you import a file |
| `read-ai` | Read.ai | Periodic | every 30 min |
| `tldv` | tl;dv | Periodic | every 30 min |
| `webex` | Webex | Periodic | every 30 min |
| `zoom` | Zoom | Periodic | every 15 min |
| `feedly` | Feedly | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `hypothesis` | Hypothesis | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `inoreader` | Inoreader | Periodic | every 30 min; timer only advances when it actually runs, so re-enabling fires immediately |
| `instapaper` | Instapaper | Import | manual; runs when you import a file |
| `kindle` | Kindle Highlights | Import | manual; runs when you import a file |
| `linkding` | Linkding | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `matter` | Matter | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `netnewswire` | NetNewsWire | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `omnivore` | Omnivore (historical import) | Import | manual; runs when you import a file |
| `pinboard` | Pinboard | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
| `pocket` | Pocket (historical import) | Import | manual; runs when you import a file |
| `raindrop` | Raindrop.io | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `readwise` | Readwise + Readwise Reader | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `reeder` | Reeder | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `snipd` | Snipd | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `wallabag` | Wallabag | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `google-takeout` | Google Takeout (My Activity) | Import | manual; runs when you import a file |
| `kagi` | Kagi | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `habitica` | Habitica | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `habitify` | Habitify | Not wired | nothing runs yet |
| `streaks` | Streaks | Unavailable | nothing runs — catalogued with the reason shown in-app |
| `way-of-life` | Way of Life | Import | manual; runs when you import a file |
| `qbserve` | Qbserve | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `timing` | Timing | Periodic | hourly; timer only advances when it actually runs, so re-enabling fires immediately |
| `wakatime` | WakaTime | Periodic | every 24 hours; timer only advances when it actually runs, so re-enabling fires immediately |
