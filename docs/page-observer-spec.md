# Page observer — opt-in content-script arm of the browser watcher

*Spec'd 2026-06-11 with David. Status: built 2026-06-11 (steps 1–4 of the build order; live validation pending). First feature: ad observation (Tier 1 display ads). This is the "deliberate opt-in" crossing the "no page content" line that `docs/data-sources.md` §browser-watcher icebox calls for — a separately-labeled collector, never the silent watcher.*

## Summary

A second, **off-by-default** arm of the Trove browser extension that injects content scripts to observe page-level signals the tabs API cannot see. The first feature is **ad observation**: which ads, from which networks and advertisers, were served to the user, and how long each was actually viewed. The foundation (dynamic script registration, optional permissions, per-feature toggles, a tagged message protocol, troved-side gating) is shared, so the other icebox features — reading/scroll depth, media metadata, page metadata (`og:`/author/word-count), selected text — plug in later as additional observers + toggles without rework.

**Scope decisions (David, 2026-06-11):** ads + extensible foundation; metadata only (no creative images in v1); capture + trove-core read APIs (no app UI yet); controls live in the extension's options page.

**Non-goals (v1):** no ad *blocking* (pure observation); no creative image/screenshot capture; no webRequest tracker census; no YouTube video ads (Tier 2) or Meta/TikTok native feed ads (Tier 3 — adversarial DOM, deliberately skipped); no Safari arm yet (follows the same path as the watcher's Safari arm when that lands).

## Hard line: the dumb sensor is untouched

The existing snapshot watcher (`background.js` → `ExtSnapshot` → `TabTracker`) keeps its exact contract: `tabs` + `nativeMessaging` + `alarms` + `webNavigation`, no page access, no content scripts. With every page-observer feature off, the extension is **byte-for-byte today's watcher** — same permissions surface (`chrome://extensions` shows "Site access: none"), same messages, same cost.

## Permissions model — optional host permissions (the enforcement, not just a toggle)

Manifest changes:

```json
"permissions": ["tabs", "nativeMessaging", "alarms", "webNavigation", "scripting", "storage"],
"optional_host_permissions": ["<all_urls>"],
"options_page": "options.html"
```

- `scripting` and `storage` are install-time but grant nothing by themselves — `chrome.scripting` cannot inject anywhere without host permissions, and the extension ships with **none**.
- Enabling page observation in the options page calls `chrome.permissions.request({origins: ["<all_urls>"]})` from the click handler (user-gesture requirement) → Chrome shows its own consent prompt → only then are content scripts registered via `chrome.scripting.registerContentScripts()`.
- Disabling unregisters all observer scripts **and** calls `chrome.permissions.remove()`. Off means the extension provably cannot read pages — verifiable in `chrome://extensions`, and no code drift can silently re-enable it without a new Chrome-mediated prompt.
- Chrome's built-in per-site access control (all sites / specific sites / on click) works on top for free: "observe everywhere except my bank" needs nothing from us.
- Re-registration is reconciled in the service worker on startup and on `chrome.storage.onChanged`: desired state (storage flags) vs. actual (`chrome.scripting.getRegisteredContentScripts()`), so a worker restart or browser update can't leave scripts half-registered. Scripts register with `persistAcrossSessions: true`.

Toggle state lives in `chrome.storage.local`:

```js
{ pageObserver: { enabled: false, features: { ads: false /* future: reading, media, pageMeta, selection */ } } }
```

Master `enabled` drives the permission grant; per-feature flags drive which scripts are registered. Each future feature gets its own flag — selected-text capture especially stays independently switchable.

## Ad observation — design

### Detection (top-frame detector, `observer/detector.js`)

Registered on `<all_urls>`, **top frame only** (`allFrames: false`), `runAt: 'document_idle'`, isolated world. Display ads live overwhelmingly in iframes served from a small set of ad-network domains. Detection signals, checked against each iframe element (readable from the parent without injecting into the frame):

1. **`src` domain** matches the compiled-in ad-domain set (see *Filter list* below).
2. **`id`/`name` attribute** matches known ad-slot patterns (`google_ads_iframe_*`, `aswift_*`, `div-gpt-ad-*` containers) — catches Google ads rendered into `srcdoc`/`about:blank` frames whose `src` carries no domain.

Discovery: one scan at `document_idle`, then a **childList-only** `MutationObserver` on `document.body` (no attribute/characterData watching), debounced 500 ms, that examines only added subtrees for new iframes. This is the one place the observer touches a page-wide observer; the debounce + childList-only scope is a hard constraint (see *Performance contract*).

### Viewability (the "time viewed" metric)

One `IntersectionObserver` (thresholds `[0, 0.5]`) over all detected ad iframes. An ad accrues `viewed_secs` while **≥50% of its pixels are in the viewport AND `document.visibilityState === 'visible'`** — the visibility gate matters because IntersectionObserver does not fire when a tab is backgrounded. Accumulation pauses/resumes on `visibilitychange`. `viewable: true` once it has been ≥50% visible for ≥1 continuous second — the MRC display-ad standard, so we measure ourselves with the industry's own ruler. Timestamps come from the observer callbacks; no polling.

An ad record opens at detection and closes when its iframe is removed from the DOM, replaced (ad refresh = new record), or the page unloads (`pagehide`).

### Advertiser extraction (ad-frame inspector, `observer/inspector.js`)

The click-through URL — which identifies the *advertiser*, not just the network — lives inside the ad's cross-origin iframe, unreadable from the parent. Rather than `allFrames: true` on `<all_urls>` (the cost multiplier the performance analysis ruled out), a **second registered script** injects only into ad-served frames:

- `matches`: the ad-domain set expressed as match patterns (`https://googleads.g.doubleclick.net/*`, `https://tpc.googlesyndication.com/*`, …), `allFrames: true`, `runAt: 'document_idle'`.
- Injection cost lands only on actual ad frames (small documents), not the page's 50+ other iframes.
- The inspector reads anchor `href`s in its frame, extracts the landing URL, and sends `{kind:'ad-frame-info', frameUrl, landingUrl, whyUrl}` to the service worker. Extraction lives in the shared `observer/ad-links.js` (`troveScanLinks`/`troveLandingFrom`/`troveWhyFrom`): it **recursively unwraps** nested click redirects (Google's `adurl=`, then second-hop redirectors like `dts.innovid.com/clktru?...&click=<advertiser>`), rejects the AdChoices/"Why this ad?" link as an advertiser (captured separately as `whyUrl`), and yields `''` rather than a network's own domain when a chain dead-ends on a tracker.
- The service worker **joins** inspector reports to detector records by `(tabId, frameUrl)` before forwarding — one merged event downstream. Unjoined detector records ship without `landing_url` after close (the join is enrichment, never a gate).

**Friendly-iframe creatives (top-frame read):** ads rendered into same-origin `about:blank`/`srcdoc` "friendly" iframes (GPT's `google_ads_iframe_*`, usually slot-detected with no ad-domain `src`) are unreachable by the inspector, but their click-through anchor is readable directly from the parent. The detector reads it via `el.contentDocument` using the same `troveScanLinks`, retried a few times as the creative renders, and attaches `landing_url`/`why_url` to the record (no join, no network). Cross-origin/sandboxed frames return `null` and stay unattributed — `network` falls back to the slot pattern (e.g. `google`). Honest fields beat guessed ones.

**Advertiser identity (opt-in, `browser-ads-identify`):** when a click chain names no advertiser but a `whyUrl` is present, the host's resolver (off by default) fetches Google's public ad-transparency page and reads the "Paid for by" payer. A landing-derived advertiser domain always wins over this — the brand domain (`apple.com`) is clearer than the legal payer (often an agency). This is the only networked path in Ads; see `crates/trove-core/src/ads.rs`.

### Filter list (standalone rule)

A curated **domain set, checked into the repo** as a generated module (`extension/observer/ad-domains.js`, a few hundred eTLD+1s: doubleclick.net, googlesyndication.com, adnxs.com, criteo.com, taboola.com, outbrain.com, amazon-adsystem.com, rubiconproject.com, …) plus the slot-pattern list. Derived offline from EasyList's ad-server sections by a maintenance script (`scripts/gen-ad-domains.*`) that is run manually and its output committed — **no runtime fetch, no full EasyList engine**. Matching is a hash-set lookup on the iframe src's registrable domain. The same module generates the inspector's match patterns, so detector and inspector can't drift.

## Protocol — tagged messages on the existing port

The wire already carries `type: 'snapshot'` (the host currently ignores the field). The host's parse becomes a `#[serde(tag = "type")]` enum:

```
{type:"snapshot", ...}   → ExtSnapshot → TabTracker (unchanged)
{type:"ads", events:[AdEvent, ...]}   → append to ads stream
```

Unknown types are logged and skipped (today's unparseable-message path already behaves this way), so old hosts tolerate new extensions and vice versa — no lockstep upgrade.

Content scripts batch locally and send to the service worker (`chrome.runtime.sendMessage`) on: record close, `pagehide`/`visibilitychange`→hidden, or a 30 s dirty timer. The service worker forwards over the native port. Buffering: content scripts cap at 100 pending events (drop oldest beyond — observational data, and unlike visits there is no history-import backup for ads; a dropped batch is acceptable by design). If the port is down (troved not installed), ad events are dropped — same idle-and-retry posture as snapshots. Runtime messages also reset the MV3 idle timer, so no new keepalive concerns.

## Vault stream + Rust side

**Stream:** `browser/ads/YYYY-MM-DD.jsonl`, keyed by record-close day. One line per ad record:

```json
{"ts":"2026-06-11T10:23:45-07:00","end":"2026-06-11T10:24:10-07:00",
 "page_url":"https://example.com/article","frame_url":"https://googleads.g.doubleclick.net/...",
 "landing_url":"https://advertiser.com/promo?...","network":"doubleclick.net","advertiser":"advertiser.com",
 "viewed_secs":12.5,"viewable":true,"w":300,"h":250,"source":"extension"}
```

- `network` / `advertiser` are derived **in trove-core at append time** from `frame_url` / `landing_url` via the existing `domain_of` — the extension stays a dumb sensor shipping raw URLs; derivation logic lives where it's tested and reusable.
- `landing_url`, `advertiser` optional (skip-serialized when empty), `viewable`/`viewed_secs` skip-serialized when false/0 — same conventions as `BrowserVisit`.
- Volume: ~50–300 records/day × ~300 B ≈ 15–100 KB/day (~1–3 MB/month) on top of the ~300 KB/day browser stream.

**`crates/trove-core/src/ads.rs`:**
- `AdEvent` (wire shape) and `AdRecord` (stored shape) structs.
- `Vault::append_ad_events(&[AdEvent])` — per-day-file **flock**, same as `append_browser_visits` and for the same reason: one host process per Chrome profile writes concurrently.
- Reads mirror the browser/activity trio: `ads_timeline(date)` (a day's records), `ads_summary(from, to)` (per-advertiser and per-network: count, viewable count, total `viewed_secs`), `ads_daily(from, to)` (`SeriesPoint`s — ads seen and ad-viewing seconds per day, for trends). No derived index yet — volume is tiny; aggregate on the fly like activity.

**`crates/troved/src/native_host.rs`:** the tagged-enum dispatch above; `ads` messages are appended directly (no state machine — viewability accrual happened browser-side; the host only stamps arrival time as a sanity bound). **Defense in depth:** ads handling is gated on its own integrations-hub flag (`integration_enabled("browser-ads")`), separate from `"browser-extension"` — even a confused extension can't write a stream the vault owner turned off. The hub flag is the Rust-side mirror of the extension-side toggle, not a replacement for it.

## Options page

`extension/options.html` + `options.js`, plain DOM, no framework (matches the extension's zero-dependency style):

- Master toggle **"Page observation"** — drives `permissions.request`/`remove` + script (un)registration. Copy states plainly what it grants: *"Allows the extension to see page content on all sites. Off: the extension cannot read any page."*
- Per-feature toggle **"Ad observation"** (disabled until master is on) — copy: *"Records which ads are shown to you (network, advertiser, landing link) and how long each was on screen. Never blocks ads, never captures page text."*
- Status line: permission state as Chrome reports it (`permissions.contains`) — the UI reflects ground truth, not stored intent.
- Future observers (reading depth, media metadata, page metadata, selected text) appear here as additional per-feature toggles.

Trove-app-side controls are out of scope (decision above); the integrations hub already exposes the troved-side gate if needed.

## Performance contract (hard constraints — encode in code review like the dumb-sensor contract)

1. **Top-frame-only** detector; the only `allFrames` script is the inspector, whose `matches` are ad domains exclusively.
2. **No polling, no scroll listeners, no layout reads in hot paths.** Viewability is IntersectionObserver-only; scroll-depth (future) likewise.
3. The only page-wide MutationObserver is **childList-only and debounced ≥500 ms**, scanning added nodes for iframes and nothing else. Future observers needing mutation watching must scope to a specific element (e.g. the YouTube player), never the document.
4. **Batched sends** (close / pagehide / 30 s dirty), never per-event messages.
5. Domain matching is a **set lookup** — no regex-list engines in content scripts.
6. Budget: ≤3 ms added work per page load, ~0 steady-state CPU between intersection changes, no retained DOM references after a record closes (leak guard for long-lived tabs).

## Privacy contract

- Off by default; enabling is a Chrome-mediated permission grant, revocable the same way, with per-site exclusions via Chrome's own site-access UI.
- The ads feature captures **URLs and geometry only** — never page text, never form content, never the creative image (v1). Future observers that do touch content (selected text) are separate toggles with their own copy.
- Data flows only over the local native-messaging port into the vault; local-only like everything else.

## Failure modes

- **MV3 worker restart:** detector state is per-page (content scripts survive worker restarts); pending joins in the worker are lost — affected records ship without `landing_url`. Acceptable, matching the dumb-sensor cost model.
- **Content script outlives a dead worker:** `sendMessage` fails, batch retried at next trigger, capped buffer.
- **SPA navigations:** records are per-document; `page_url` is captured at detection time. Ad refreshes on SPA route changes appear naturally as new iframe records.
- **Malicious page spoofing:** a page can fabricate ad-shaped iframes in its own DOM; it cannot forge extension messages (routing is real content script → worker → port). Fabricated-looking ads in a hostile page's own records are inherent to DOM observation and acceptable for personal analytics.

## Testing

- **Rust:** fixture tests for `ads.rs` (append/read round-trip, domain derivation, optional-field serde compat) and the host's tagged dispatch (framed mixed `snapshot`/`ads`/unknown messages piped in — extends the session-6 smoke-test pattern against a temp vault).
- **Extension:** a checked-in fixture page (`extension/test/fixtures/ads.html`) with fake ad iframes (matching `src` domains and slot-pattern ids, one below the fold) for manual smoke: scroll, verify viewability timing, close tab, verify the vault rows.
- **Live validation:** same bar as the watcher — install in real Chrome, browse an ad-heavy site, inspect `browser/ads/`.

## Build order

1. **Foundation:** manifest changes, options page, permission grant/revoke + script-registration reconciler. *(S)*
2. **Domain list:** generator script + committed `ad-domains.js`. *(S)*
3. **Detector:** iframe detection + viewability + batching; host dispatch + `ads.rs` append/reads + hub gate. First end-to-end data. *(M)*
4. **Inspector:** ad-frame script + worker-side join → `landing_url`/`advertiser` enrichment. *(S–M)*
5. Smoke + live validation; update `docs/data-sources.md` status. *(S)*

Steps 1–3 are a shippable v1 (network-level attribution + viewability); step 4 adds advertiser attribution.

## Future (explicitly out of v1)

Creative image capture (vault-size + cross-origin tradeoffs); webRequest tracker census (needs tight URL filters or it keeps the worker hot); YouTube ad breaks (rides the future media-metadata observer); native in-DOM sponsored widgets (Taboola/Outbrain anchors outside iframes); Meta feed ads (adversarial, revisit only deliberately); Safari arm; the remaining icebox observers (reading depth, page metadata, selected text) as new per-feature toggles on this foundation.
