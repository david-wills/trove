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
| `macos-screenshots` | Screenshots | Live | always on; ticked every poll (5 s) |
| `apple-mail` | Apple Mail | Periodic | every 15 min |
| `dropbox` | Dropbox | Periodic | every 15 min |
| `google-drive` | Google Drive | Periodic | every 15 min |
| `icloud-drive` | iCloud Drive | Periodic | every 15 min |
| `macos-downloads` | macOS Downloads | Periodic | hourly |
| `fathom` | Fathom | Periodic | every 15 min |
