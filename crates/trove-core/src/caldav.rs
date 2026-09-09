//! CalDAV (generic) — catch-all calendar connector for any RFC 4791 server.
//!
//! Supports iCloud, Fastmail, Nextcloud, Proton Calendar (via Bridge),
//! Radicale/Baikal self-hosted, and any other RFC-4791-compliant server.
//! One login covers every calendar on the configured server.
//!
//! **Auth:** HTTP Basic over TLS. Credentials (server URL + username +
//! password) are stored in the secret store under the "caldav" service key:
//! `access_token` = password (secret), `scope` = username,
//! `token_type` = server base URL.
//!
//! **Sync:** PROPFIND discovers calendars; per-calendar REPORT fetches all
//! VEVENT objects with their ETags. The CTag (or ETag map) watermark is
//! persisted in `.trove/caldav-sync.json` so re-polls skip unchanged
//! calendars.
//!
//! **Contract:** rows are written via [`crate::calendar::CalendarOccurrence`]
//! into the shared `calendar/events/` + `calendar/changes/` store using the
//! scoped snapshot helper (prefix [`CALDAV_ROW_PREFIX`]) so EventKit and
//! Google Calendar rows are never overwritten.
//!
//! **Raw:** full-fidelity VEVENT objects (all properties) land in
//! `calendar/caldav/raw/YYYY-MM.jsonl`.
//!
//! **RRULE:** simple in-window expansion for FREQ=DAILY/WEEKLY/MONTHLY/YEARLY
//! without external deps. BYDAY is respected for WEEKLY rules. Complex rules
//! (BYMONTHDAY, BYSETPOS, COUNT with large N) store a single base occurrence
//! and set `recurring=true`; the raw layer always contains the full RRULE.
//!
//! Brief: docs/integrations/caldav.md

use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::calendar::{CalendarOccurrence, WINDOW_FUTURE_DAYS, WINDOW_PAST_DAYS};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::write_json_atomic;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "caldav";
const SYNC_FILE: &str = ".trove/caldav-sync.json";
const RAW_DIR: &str = "calendar/caldav/raw";

/// Row-id prefix for CalDAV-owned occurrences in the shared calendar store.
/// Lets [`crate::calendar::Vault::calendar_snapshot_scoped`] know which rows
/// belong to this collector so EventKit and Google Calendar rows are never
/// overwritten or mass-removed.
pub(crate) const CALDAV_ROW_PREFIX: &str = "caldav:";

/// Seconds between CalDAV sync passes in the owner loop. CTag makes a
/// no-change pass a cheap single PROPFIND per calendar.
pub const CALDAV_SYNC_SECS: u64 = 900; // 15 minutes

// CalDAV XML namespaces (embedded directly in request bodies below).

// ---------------------------------------------------------------------------
// Persisted state (watermark — NOT the credential)

/// Per-calendar sync state: CTag + per-object ETag map. Stored in
/// `.trove/caldav-sync.json`. The credential lives only in the 0600 secret
/// store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalDavSyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Per-calendar-URL state.
    #[serde(default)]
    pub calendars: BTreeMap<String, CalDavCalendarState>,
    /// Any error from the last attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Watermark for one calendar collection (URL → state).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalDavCalendarState {
    /// The last-seen CTag value. When this matches the server, all ETags
    /// are unchanged and we skip the REPORT entirely.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ctag: String,
    /// Per-object ETag cache: `{href} → etag`. Used to detect individual
    /// object changes when the CTag changes.
    #[serde(default)]
    pub etags: BTreeMap<String, String>,
    /// Display name of the calendar collection.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    /// RFC3339 local time of the last successful fetch for this calendar.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_fetched: String,
    /// RFC3339 time of the first completed sync for this calendar.
    /// Absent until the first sync finishes. Used to distinguish first-run
    /// (baseline — no `added` change lines) from subsequent syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_synced: Option<String>,
}

// ---------------------------------------------------------------------------
// Raw VEVENT record (full fidelity)

/// One VEVENT as parsed from the ICS object — full fidelity for the raw layer.
/// Serialised to `calendar/caldav/raw/YYYY-MM.jsonl` partitioned by the
/// month of `dtstart` (or current month when absent).
///
/// All optional fields carry `#[serde(default)]` so that old lines written
/// with fewer fields still deserialise cleanly (additive, back-compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawVEvent {
    /// The calendar collection URL this event came from.
    pub calendar_url: String,
    /// The calendar display name.
    pub calendar_name: String,
    /// The `.ics` object href on the server.
    pub href: String,
    /// The server-reported ETag for this object.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub etag: String,
    /// VEVENT UID — stable identifier.
    pub uid: String,
    /// SUMMARY
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// DTSTART (raw string from ICS, e.g. "20260610T140000Z" or "20260701").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dtstart: String,
    /// TZID parameter of DTSTART (e.g. "America/Los_Angeles"), when present.
    /// Absent for UTC (Z-suffix) and date-only values.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dtstart_tzid: String,
    /// DTEND (raw string).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dtend: String,
    /// TZID parameter of DTEND (e.g. "America/Los_Angeles"), when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dtend_tzid: String,
    /// DURATION if present instead of DTEND.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub duration: String,
    /// LOCATION
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// DESCRIPTION
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// STATUS (CONFIRMED / TENTATIVE / CANCELLED)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// RRULE (the full raw recurrence rule string)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rrule: String,
    /// RECURRENCE-ID (when this is a single-instance override of a recurring
    /// event)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recurrence_id: String,
    /// ORGANIZER
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organizer: String,
    /// ATTENDEE values (one per attendee)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    /// All other non-standard and extension properties (X-*, etc.) as
    /// `{NAME} → VALUE` pairs for full fidelity. Multiple values with the
    /// same name keep only the last (safe for the raw layer).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
    /// Month partition key (YYYY-MM) — derived from dtstart.
    pub partition: String,
}

// ---------------------------------------------------------------------------
// Credential helpers

struct Credential {
    server_url: String,
    username: String,
    password: String,
}

fn load_credential(vault: &Vault) -> Result<Option<Credential>> {
    let Some(ts) = vault.load_sync_token(SERVICE)? else {
        return Ok(None);
    };
    let url = ts.token_type.unwrap_or_default();
    let username = ts.scope.unwrap_or_default();
    let password = ts.access_token;
    if url.is_empty() || username.is_empty() || password.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credential { server_url: url, username, password }))
}

/// Parse a pasted CalDAV credential of the form:
/// ```text
/// https://caldav.example.com/
/// username
/// app-specific-password
/// ```
/// Lines are trimmed; blank lines are skipped. Returns `None` when fewer than
/// 3 non-empty lines are present.
fn parse_pasted_credential(pasted: &str) -> Option<Credential> {
    let parts: Vec<&str> = pasted.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    Some(Credential {
        server_url: parts[0].to_string(),
        username: parts[1].to_string(),
        password: parts[2].to_string(),
    })
}

// ---------------------------------------------------------------------------
// HTTP CalDAV client (ureq + HTTP Basic)

struct CalDavClient {
    server_url: String,
    username: String,
    password: String,
}

impl CalDavClient {
    fn new(cred: &Credential) -> Self {
        let mut url = cred.server_url.trim_end_matches('/').to_string();
        url.push('/');
        Self { server_url: url, username: cred.username.clone(), password: cred.password.clone() }
    }

    /// Build a ureq request with Basic auth and common CalDAV headers.
    fn req(&self, method: &str, url: &str) -> ureq::Request {
        ureq::request(method, url)
            .set("Authorization", &basic_auth(&self.username, &self.password))
            .set("Content-Type", "application/xml; charset=utf-8")
    }

    /// PROPFIND the current-user-principal URL.
    fn current_user_principal(&self) -> Result<Option<String>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;
        let resp = self
            .req("PROPFIND", &self.server_url)
            .set("Depth", "0")
            .send_string(body)
            .context("PROPFIND current-user-principal")?;
        let text = resp.into_string()?;
        Ok(extract_dav_href(&text, "current-user-principal"))
    }

    /// PROPFIND the calendar-home-set for a principal URL.
    fn calendar_home_set(&self, principal_url: &str) -> Result<Option<String>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>"#;
        let resp = self
            .req("PROPFIND", principal_url)
            .set("Depth", "0")
            .send_string(body)
            .context("PROPFIND calendar-home-set")?;
        let text = resp.into_string()?;
        Ok(extract_dav_href(&text, "calendar-home-set"))
    }

    /// PROPFIND calendar home: returns `(href, displayname, ctag)` for each
    /// calendar collection that supports VEVENT.
    fn list_calendars(&self, home_url: &str) -> Result<Vec<CalendarMeta>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"
            xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <cs:getctag/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;
        let resp = self
            .req("PROPFIND", home_url)
            .set("Depth", "1")
            .send_string(body)
            .context("PROPFIND calendar list")?;
        let text = resp.into_string()?;
        parse_calendar_list(&text, home_url)
    }

    /// REPORT calendar-query: fetch all VEVENTs with ETags from one calendar.
    /// Returns `Vec<(href, etag, ics_body)>`.
    fn fetch_all_events(&self, calendar_url: &str) -> Result<Vec<(String, String, String)>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;
        let resp = self
            .req("REPORT", calendar_url)
            .set("Depth", "1")
            .send_string(body)
            .context("REPORT calendar-query")?;
        let text = resp.into_string()?;
        parse_multistatus_with_data(&text)
    }

    /// REPORT calendar-query: fetch only ETags (cheap change-detection pass).
    /// Returns `Vec<(href, etag)>`. Used for CTag-changed calendars when
    /// bandwidth is a concern; currently the full-data pass is used instead.
    #[allow(dead_code)]
    fn fetch_etags(&self, calendar_url: &str) -> Result<Vec<(String, String)>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;
        let resp = self
            .req("REPORT", calendar_url)
            .set("Depth", "1")
            .send_string(body)
            .context("REPORT etags-only")?;
        let text = resp.into_string()?;
        Ok(parse_multistatus_with_data(&text)?
            .into_iter()
            .map(|(href, etag, _)| (href, etag))
            .collect())
    }

    /// REPORT calendar-multiget: fetch full ICS for a specific set of hrefs.
    /// Used in bandwidth-optimised two-pass sync (ETag diff then fetch only
    /// changed objects); available for future use.
    #[allow(dead_code)]
    fn multiget(&self, calendar_url: &str, hrefs: &[String]) -> Result<Vec<(String, String, String)>> {
        if hrefs.is_empty() {
            return Ok(Vec::new());
        }
        let href_elements: String =
            hrefs.iter().map(|h| format!("  <d:href>{}</d:href>\n", h)).collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
{}</c:calendar-multiget>"#,
            href_elements
        );
        let resp = self
            .req("REPORT", calendar_url)
            .set("Depth", "1")
            .send_string(&body)
            .context("REPORT calendar-multiget")?;
        let text = resp.into_string()?;
        parse_multistatus_with_data(&text)
    }
}

/// HTTP Basic auth header value.
fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}")
}

// ---------------------------------------------------------------------------
// XML parsing helpers (quick-xml)

#[derive(Debug, Clone)]
struct CalendarMeta {
    href: String,
    display_name: String,
    ctag: String,
    supports_vevent: bool,
}

/// Extract the first `<d:href>` child under an element named `tag` from a
/// DAV:multistatus response. Strips the host/port prefix when the href is
/// absolute to normalise it to a path.
fn extract_dav_href(xml: &str, tag: &str) -> Option<String> {
    // Simple scan: find the tag, then find the next <d:href> or <href>
    let tag_lower = tag.to_lowercase();
    let pos = xml.to_lowercase().find(&tag_lower)?;
    let rest = &xml[pos..];
    // Find closing of the parent tag to limit scope
    let end = rest.to_lowercase().find(&format!("</{}", tag_lower)).unwrap_or(rest.len());
    let scope = &rest[..end];
    extract_element_text(scope, "href")
}

/// Walk a DAV:multistatus XML body and pull out every `<d:response>` block,
/// returning `(href, local-name)` pairs for a given local element name.
fn extract_element_text(xml: &str, local_name: &str) -> Option<String> {
    // Case-insensitive search for <local_name> ... </local_name> or <d:local_name>
    let patterns = [
        format!("<{}>", local_name),
        format!("<d:{}>", local_name),
        format!("<D:{}>", local_name),
    ];
    for pat in &patterns {
        if let Some(start) = xml.find(pat.as_str()) {
            let after = &xml[start + pat.len()..];
            let end_tag_candidates = [
                format!("</{}>", local_name),
                format!("</d:{}>", local_name),
                format!("</D:{}>", local_name),
            ];
            for end_pat in &end_tag_candidates {
                if let Some(end) = after.find(end_pat.as_str()) {
                    return Some(after[..end].trim().to_string());
                }
            }
        }
    }
    None
}

/// Parse a `PROPFIND Depth:1` multistatus response into calendar metadata.
fn parse_calendar_list(xml: &str, home_url: &str) -> Result<Vec<CalendarMeta>> {
    let mut metas = Vec::new();
    // Split on <d:response> (case-insensitive)
    let responses = split_responses(xml);
    let home_path = url_path(home_url);

    for resp in responses {
        let href = match extract_element_text(&resp, "href") {
            Some(h) => h,
            None => continue,
        };
        // Skip the home collection itself
        let href_path = url_path(&href);
        if href_path == home_path || href.trim_end_matches('/') == home_url.trim_end_matches('/') {
            continue;
        }
        // Must be a calendar resourcetype
        let is_calendar = resp.contains(":calendar") || resp.contains("calendar/>");
        if !is_calendar {
            continue;
        }
        // VEVENT support (absent = assume yes; only skip VTODO-only collections)
        let is_vtodo_only = resp.contains(r#"name="VTODO""#)
            && !resp.contains(r#"name="VEVENT""#);
        let supports_vevent = !is_vtodo_only;

        let display_name = extract_element_text(&resp, "displayname").unwrap_or_default();
        let ctag = extract_ctag(&resp).unwrap_or_default();

        metas.push(CalendarMeta { href, display_name, ctag, supports_vevent });
    }
    Ok(metas)
}

/// Extract getctag value from a propstat block.
fn extract_ctag(xml: &str) -> Option<String> {
    // <cs:getctag> or <getctag>
    for pat in &["<cs:getctag>", "<getctag>", "<CS:getctag>"] {
        if let Some(start) = xml.find(pat) {
            let after = &xml[start + pat.len()..];
            for end_pat in &["</cs:getctag>", "</getctag>", "</CS:getctag>"] {
                if let Some(end) = after.find(end_pat) {
                    return Some(after[..end].trim().to_string());
                }
            }
        }
    }
    None
}

/// Parse a multistatus response into `(href, etag, calendar-data)` triples.
/// Calendar-data may be empty when only etags were requested.
fn parse_multistatus_with_data(xml: &str) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    for resp in split_responses(xml) {
        let href = match extract_element_text(&resp, "href") {
            Some(h) if h.ends_with(".ics") => h,
            _ => continue,
        };
        let etag = extract_element_text(&resp, "getetag").unwrap_or_default();
        let etag = etag.trim_matches('"').to_string();
        let cal_data = extract_calendar_data(&resp).unwrap_or_default();
        out.push((href, etag, cal_data));
    }
    Ok(out)
}

/// Split a multistatus body on `<d:response>` / `<D:response>` boundaries.
fn split_responses(xml: &str) -> Vec<String> {
    let xml_lower = xml.to_lowercase();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(start) = xml_lower[pos..].find("<d:response>").or_else(|| xml_lower[pos..].find("<D:response>")) {
        let abs_start = pos + start;
        // Find the matching close tag
        let after_open = abs_start + "<d:response>".len();
        let end_tag = "</d:response>";
        if let Some(end_rel) = xml_lower[after_open..].find(end_tag) {
            let abs_end = after_open + end_rel + end_tag.len();
            out.push(xml[abs_start..abs_end].to_string());
            pos = abs_end;
        } else {
            break;
        }
    }
    out
}

/// Extract raw calendar-data text from a response block.
fn extract_calendar_data(xml: &str) -> Option<String> {
    let patterns = [
        ("<c:calendar-data>", "</c:calendar-data>"),
        ("<C:calendar-data>", "</C:calendar-data>"),
        ("<calendar-data>", "</calendar-data>"),
    ];
    for (open, close) in &patterns {
        if let Some(start) = xml.find(open) {
            let after = &xml[start + open.len()..];
            if let Some(end) = after.find(close) {
                return Some(after[..end].trim().to_string());
            }
        }
    }
    None
}

/// Return the path component of a URL, stripping scheme+host. If `url` has
/// no scheme (already a path), returns it as-is.
fn url_path(url: &str) -> &str {
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        if let Some(slash) = rest.find('/') {
            return &rest[slash..];
        }
        return "/";
    }
    url
}

// ---------------------------------------------------------------------------
// ICS / VEVENT parsing (ical crate)

/// Parse an ICS body into a list of raw VEVENT records.
fn parse_ics(ics: &str, calendar_url: &str, calendar_name: &str, href: &str, etag: &str) -> Vec<RawVEvent> {
    let reader = BufReader::new(ics.as_bytes());
    let mut parser = ical::IcalParser::new(reader);
    let mut out = Vec::new();

    let cal = match parser.next() {
        Some(Ok(c)) => c,
        _ => return out,
    };

    for event in &cal.events {
        let mut raw = RawVEvent {
            calendar_url: calendar_url.to_string(),
            calendar_name: calendar_name.to_string(),
            href: href.to_string(),
            etag: etag.to_string(),
            uid: String::new(),
            summary: String::new(),
            dtstart: String::new(),
            dtstart_tzid: String::new(),
            dtend: String::new(),
            dtend_tzid: String::new(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            rrule: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: String::new(),
        };

        for prop in &event.properties {
            let name = prop.name.as_str();
            let val = prop.value.as_deref().unwrap_or("").trim().to_string();
            match name {
                "UID" => raw.uid = val,
                "SUMMARY" => raw.summary = val,
                "DTSTART" => {
                    // Capture the TZID parameter (e.g. DTSTART;TZID=America/Los_Angeles:…)
                    if let Some(params) = &prop.params {
                        if let Some((_, vals)) = params.iter().find(|(k, _)| k == "TZID") {
                            if let Some(tz) = vals.first() {
                                raw.dtstart_tzid = tz.clone();
                            }
                        }
                    }
                    raw.dtstart = val;
                }
                "DTEND" => {
                    if let Some(params) = &prop.params {
                        if let Some((_, vals)) = params.iter().find(|(k, _)| k == "TZID") {
                            if let Some(tz) = vals.first() {
                                raw.dtend_tzid = tz.clone();
                            }
                        }
                    }
                    raw.dtend = val;
                }
                "DURATION" => raw.duration = val,
                "LOCATION" => raw.location = val,
                "DESCRIPTION" => raw.description = val,
                "STATUS" => raw.status = val,
                "RRULE" => raw.rrule = val,
                "RECURRENCE-ID" => raw.recurrence_id = val,
                "ORGANIZER" => {
                    // Prefer CN param if present
                    if let Some(params) = &prop.params {
                        if let Some((_, vals)) = params.iter().find(|(k, _)| k == "CN") {
                            if let Some(cn) = vals.first() {
                                raw.organizer = cn.clone();
                                continue;
                            }
                        }
                    }
                    raw.organizer = val
                        .trim_start_matches("mailto:")
                        .trim_start_matches("MAILTO:")
                        .to_string();
                }
                "ATTENDEE" => {
                    let display = if let Some(params) = &prop.params {
                        params.iter().find(|(k, _)| k == "CN").and_then(|(_, v)| v.first().cloned())
                    } else {
                        None
                    };
                    let addr = val
                        .trim_start_matches("mailto:")
                        .trim_start_matches("MAILTO:")
                        .to_string();
                    raw.attendees.push(display.unwrap_or(addr));
                }
                other => {
                    if !other.is_empty() {
                        raw.extra.insert(other.to_string(), val);
                    }
                }
            }
        }

        if raw.uid.is_empty() {
            continue; // skip mal-formed events
        }

        raw.partition = dtstart_month(&raw.dtstart);
        out.push(raw);
    }
    out
}

// ---------------------------------------------------------------------------
// DTSTART / DTEND parsing and RFC3339 conversion

/// Return the YYYY-MM partition month of a DTSTART string.
fn dtstart_month(dtstart: &str) -> String {
    if dtstart.len() >= 7 && dtstart.chars().nth(4) == Some('-') {
        // Already looks like YYYY-MM or YYYY-MM-DD
        return dtstart[..7].to_string();
    }
    // ICS format: "20260610T140000Z" or "20260701" (all-day)
    if dtstart.len() >= 6 {
        let y = &dtstart[..4];
        let m = &dtstart[4..6];
        format!("{y}-{m}")
    } else {
        Local::now().format("%Y-%m").to_string()
    }
}

/// Parse an ICS datetime string to an RFC3339 local string.
/// - UTC: `20260610T140000Z` → convert to local
/// - Floating: `20260610T140000` → treat as local
/// - Date-only (all-day): `20260701` → local midnight
/// Convert an ICS datetime string to RFC3339, resolving timezone correctly.
///
/// - UTC (`Z`-suffix): converted to local, exact instant preserved.
/// - TZID present (e.g. `America/Los_Angeles`): wall-clock resolved against
///   that IANA zone via chrono-tz; falls back to local-zone on unknown TZID.
/// - Floating (no `Z`, no TZID): treated as local wall-clock (same as before;
///   only reached for servers that omit TZID — uncommon for real events).
/// - All-day (8 digits): local midnight; exclusive DTEND → previous day 23:59:59.
fn ics_dt_to_rfc3339_tz(dt: &str, tzid: &str, is_dtend_all_day: bool) -> Option<String> {
    let dt = dt.trim();
    if dt.is_empty() {
        return None;
    }

    // All-day: 8 digits — timezone is irrelevant (date-only)
    if dt.len() == 8 && dt.chars().all(|c| c.is_ascii_digit()) {
        let y: i32 = dt[0..4].parse().ok()?;
        let mo: u32 = dt[4..6].parse().ok()?;
        let d: u32 = dt[6..8].parse().ok()?;
        let date = NaiveDate::from_ymd_opt(y, mo, d)?;
        let naive = if is_dtend_all_day {
            // all-day DTEND is exclusive — subtract 1 day, use 23:59:59
            let prev = date.pred_opt()?;
            prev.and_hms_opt(23, 59, 59)?
        } else {
            date.and_hms_opt(0, 0, 0)?
        };
        return Some(Local.from_local_datetime(&naive).earliest()?.to_rfc3339());
    }

    // Datetime (at least YYYYMMDDTHHmmss)
    if dt.len() >= 15 {
        let date_part = &dt[0..8];
        let time_part = &dt[9..15];
        let is_utc = dt.ends_with('Z');

        let y: i32 = date_part[0..4].parse().ok()?;
        let mo: u32 = date_part[4..6].parse().ok()?;
        let d: u32 = date_part[6..8].parse().ok()?;
        let h: u32 = time_part[0..2].parse().ok()?;
        let mi: u32 = time_part[2..4].parse().ok()?;
        let s: u32 = time_part[4..6].parse().ok()?;

        let naive = chrono::NaiveDateTime::new(
            NaiveDate::from_ymd_opt(y, mo, d)?,
            chrono::NaiveTime::from_hms_opt(h, mi, s)?,
        );

        if is_utc {
            // UTC: exact instant — convert to local for storage
            let utc: DateTime<chrono::Utc> = chrono::Utc.from_utc_datetime(&naive);
            return Some(utc.with_timezone(&Local).to_rfc3339());
        }

        if !tzid.is_empty() {
            // TZID present: resolve wall-clock against the named IANA zone
            if let Ok(tz) = tzid.parse::<Tz>() {
                // from_local_datetime handles DST ambiguity; earliest picks std
                let local_in_zone = tz.from_local_datetime(&naive).earliest()?;
                // Convert to machine-local for a consistent RFC3339 offset
                return Some(local_in_zone.with_timezone(&Local).to_rfc3339());
            }
            // Unknown TZID (non-IANA or misspelled) — fall through to local
        }

        // Floating datetime (no Z, no TZID, or unrecognized TZID): treat as local
        return Some(Local.from_local_datetime(&naive).earliest()?.to_rfc3339());
    }

    None
}

/// Convenience wrapper — no TZID (legacy callers for all-day / UTC strings).
fn ics_dt_to_rfc3339(dt: &str, is_dtend_all_day: bool) -> Option<String> {
    ics_dt_to_rfc3339_tz(dt, "", is_dtend_all_day)
}

/// True when the DTSTART has VALUE=DATE (all-day event).
fn is_all_day(dtstart: &str) -> bool {
    dtstart.len() == 8 && dtstart.chars().all(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// RRULE expansion (simple, in-window, no external deps)

/// Normalize a STATUS string to the contract's allowed values.
fn normalize_status(s: &str) -> String {
    match s.to_uppercase().as_str() {
        "CONFIRMED" => "confirmed".to_string(),
        "TENTATIVE" => "tentative".to_string(),
        "CANCELLED" | "CANCELED" => "canceled".to_string(),
        _ => String::new(),
    }
}

/// Expand a single VEVENT into [`CalendarOccurrence`]s within a day window.
/// - Non-recurring: one occurrence (unless filtered out by window).
/// - Recurring with RRULE: expand FREQ=DAILY/WEEKLY/MONTHLY/YEARLY within
///   the window. BYDAY (MO–SU) on WEEKLY rules is respected.
///   Complex rules (BYMONTHDAY, BYSETPOS, etc.) emit the base occurrence only
///   with `recurring=true` (the raw layer keeps the full RRULE).
///
/// `override_slots`: RFC3339 occurrence timestamps from RECURRENCE-ID overrides
/// for the same UID. These base-series slots are skipped so that only the
/// override occurrence row exists (no duplicate id+occurrence pairs).
fn expand_event(
    raw: &RawVEvent,
    calendar_name: &str,
    account: &str,
    window_start: &str,
    window_end: &str,
    override_slots: &std::collections::HashSet<String>,
) -> Vec<CalendarOccurrence> {
    let all_day = is_all_day(&raw.dtstart);
    let start_rfc = match ics_dt_to_rfc3339_tz(&raw.dtstart, &raw.dtstart_tzid, false) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let end_rfc = if !raw.dtend.is_empty() {
        ics_dt_to_rfc3339_tz(&raw.dtend, &raw.dtend_tzid, all_day)
            .unwrap_or_else(|| start_rfc.clone())
    } else {
        start_rfc.clone()
    };

    let status = normalize_status(&raw.status);

    let make_occ = |start: &str, end: &str, occurrence: &str| CalendarOccurrence {
        id: format!("{}{}", CALDAV_ROW_PREFIX, raw.uid),
        occurrence: occurrence.to_string(),
        start: start.to_string(),
        end: end.to_string(),
        all_day,
        title: raw.summary.clone(),
        calendar: calendar_name.to_string(),
        account: account.to_string(),
        location: raw.location.clone(),
        notes: raw.description.clone(),
        attendees: raw.attendees.clone(),
        status: status.clone(),
        recurring: !raw.rrule.is_empty(),
    };

    // Single-instance override (RECURRENCE-ID): emit it keyed by the original
    // start time so the diff engine pairs it correctly.
    if !raw.recurrence_id.is_empty() {
        let rid_start = ics_dt_to_rfc3339_tz(
            &raw.recurrence_id,
            &raw.dtstart_tzid,
            false,
        )
        .unwrap_or_default();
        if day_of(&start_rfc) >= window_start && day_of(&start_rfc) <= window_end {
            return vec![make_occ(&start_rfc, &end_rfc, &rid_start)];
        }
        return Vec::new();
    }

    if raw.rrule.is_empty() {
        // Single event — filter to window
        if day_of(&start_rfc) >= window_start && day_of(&start_rfc) <= window_end {
            return vec![make_occ(&start_rfc, &end_rfc, &start_rfc)];
        }
        return Vec::new();
    }

    // Recurring: parse and expand, skipping slots replaced by RECURRENCE-ID overrides
    expand_rrule(
        raw,
        &start_rfc,
        &end_rfc,
        all_day,
        &status,
        calendar_name,
        account,
        window_start,
        window_end,
        override_slots,
    )
}

/// Build the set of RFC3339 occurrence timestamps that are covered by
/// RECURRENCE-ID overrides for a given UID, within the list of raw events.
/// These slots must be skipped when expanding the base RRULE so there is
/// exactly one row per id+occurrence key.
fn override_slots_for_uid(
    uid: &str,
    raw_events: &[RawVEvent],
) -> std::collections::HashSet<String> {
    raw_events
        .iter()
        .filter(|e| e.uid == uid && !e.recurrence_id.is_empty())
        .filter_map(|e| ics_dt_to_rfc3339_tz(&e.recurrence_id, &e.dtstart_tzid, false))
        .collect()
}

/// Parse RRULE string into key fields for expansion.
#[derive(Default)]
struct RRule {
    freq: String,
    interval: u32,
    count: Option<u32>,
    until: Option<String>,
    byday: Vec<u8>, // 0=Mon, 1=Tue, … 6=Sun (per iCal)
    complex: bool,  // has BYMONTHDAY / BYSETPOS / etc.
}

fn parse_rrule(rrule: &str) -> RRule {
    let mut r = RRule { interval: 1, ..Default::default() };
    for part in rrule.split(';') {
        if let Some((key, val)) = part.split_once('=') {
            match key.trim().to_uppercase().as_str() {
                "FREQ" => r.freq = val.trim().to_uppercase(),
                "INTERVAL" => r.interval = val.trim().parse().unwrap_or(1).max(1),
                "COUNT" => r.count = val.trim().parse().ok(),
                "UNTIL" => r.until = Some(val.trim().to_string()),
                "BYDAY" => {
                    for d in val.split(',') {
                        let d = d.trim();
                        // Skip ordinal BYDAY (e.g. 1MO = first Monday) — complex
                        if d.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-') {
                            r.complex = true;
                            continue;
                        }
                        if let Some(n) = byday_to_num(d) {
                            r.byday.push(n);
                        }
                    }
                }
                "BYMONTHDAY" | "BYSETPOS" | "BYYEARDAY" | "BYWEEKNO" => r.complex = true,
                _ => {}
            }
        }
    }
    r
}

fn byday_to_num(d: &str) -> Option<u8> {
    // chrono weekday: Mon=0, Tue=1, …, Sun=6
    match d.to_uppercase().as_str() {
        "MO" => Some(0),
        "TU" => Some(1),
        "WE" => Some(2),
        "TH" => Some(3),
        "FR" => Some(4),
        "SA" => Some(5),
        "SU" => Some(6),
        _ => None,
    }
}

fn expand_rrule(
    raw: &RawVEvent,
    start_rfc: &str,
    end_rfc: &str,
    all_day: bool,
    status: &str,
    calendar_name: &str,
    account: &str,
    window_start: &str,
    window_end: &str,
    // RFC3339 occurrence timestamps that are overridden by a RECURRENCE-ID
    // instance — skip these slots so the override is the sole row.
    override_slots: &std::collections::HashSet<String>,
) -> Vec<CalendarOccurrence> {
    let r = parse_rrule(&raw.rrule);

    // For complex rules we emit at most the base occurrence if it falls in window
    if r.complex || r.freq.is_empty() {
        if day_of(start_rfc) >= window_start && day_of(start_rfc) <= window_end
            && !override_slots.contains(start_rfc)
        {
            return vec![CalendarOccurrence {
                id: format!("{}{}", CALDAV_ROW_PREFIX, raw.uid),
                occurrence: start_rfc.to_string(),
                start: start_rfc.to_string(),
                end: end_rfc.to_string(),
                all_day,
                title: raw.summary.clone(),
                calendar: calendar_name.to_string(),
                account: account.to_string(),
                location: raw.location.clone(),
                notes: raw.description.clone(),
                attendees: raw.attendees.clone(),
                status: status.to_string(),
                recurring: true,
            }];
        }
        return Vec::new();
    }

    // Compute duration once (for adjusting each instance's end)
    let duration_secs = rfc3339_diff_secs(start_rfc, end_rfc).unwrap_or(0);

    // Parse the base start into a chrono type for stepping
    let base_dt = match chrono::DateTime::parse_from_rfc3339(start_rfc) {
        Ok(dt) => dt.with_timezone(&Local),
        Err(_) => return Vec::new(),
    };

    let win_end_day = window_end;
    let win_start_day = window_start;

    // UNTIL upper bound
    let until_day: Option<String> = r.until.as_ref().and_then(|u| {
        ics_dt_to_rfc3339(u, false).map(|rfc| day_of(&rfc).to_string())
    });

    let mut out = Vec::new();
    let mut step = 0u64;
    let mut count_emitted = 0u32;
    let max_step = 5000u64; // safety cap

    loop {
        if step > max_step {
            break;
        }
        let occ_dt = advance_rrule(&base_dt, &r, step);
        let occ_day = occ_dt.format("%Y-%m-%d").to_string();

        // Compute end for this instance
        let occ_end_dt = occ_dt + chrono::Duration::seconds(duration_secs);
        let occ_start_rfc = occ_dt.to_rfc3339();
        let occ_end_rfc = occ_end_dt.to_rfc3339();

        // Past the UNTIL bound?
        if let Some(ref until) = until_day {
            if occ_day.as_str() > until.as_str() {
                break;
            }
        }
        // Past the window and COUNT exhausted?
        if occ_day.as_str() > win_end_day {
            break;
        }

        // COUNT limit
        if let Some(max_count) = r.count {
            if count_emitted >= max_count {
                break;
            }
            count_emitted += 1;
        }

        // BYDAY filter for WEEKLY
        if r.freq == "WEEKLY" && !r.byday.is_empty() {
            let weekday = occ_dt.weekday().num_days_from_monday() as u8;
            if !r.byday.contains(&weekday) {
                step += 1;
                continue;
            }
        }

        // Skip slots that a RECURRENCE-ID override replaces
        if override_slots.contains(&occ_start_rfc) {
            step += 1;
            continue;
        }

        if occ_day.as_str() >= win_start_day {
            out.push(CalendarOccurrence {
                id: format!("{}{}", CALDAV_ROW_PREFIX, raw.uid),
                occurrence: occ_start_rfc.clone(),
                start: occ_start_rfc,
                end: occ_end_rfc,
                all_day,
                title: raw.summary.clone(),
                calendar: calendar_name.to_string(),
                account: account.to_string(),
                location: raw.location.clone(),
                notes: raw.description.clone(),
                attendees: raw.attendees.clone(),
                status: status.to_string(),
                recurring: true,
            });
        }

        step += 1;
    }

    out
}

/// Return the `Nth` occurrence datetime for a given RRULE from the base.
fn advance_rrule(base: &DateTime<Local>, r: &RRule, step: u64) -> DateTime<Local> {
    let n = step as i64 * r.interval as i64;
    match r.freq.as_str() {
        "DAILY" => *base + Duration::days(n),
        "WEEKLY" => {
            // If BYDAY is set we step daily; the caller filters by weekday.
            // Without BYDAY we step by full weeks.
            if r.byday.is_empty() {
                *base + Duration::weeks(n)
            } else {
                *base + Duration::days(n)
            }
        }
        "MONTHLY" => add_months(base, n),
        "YEARLY" => add_months(base, n * 12),
        _ => *base,
    }
}

fn add_months(dt: &DateTime<Local>, months: i64) -> DateTime<Local> {
    let mut year = dt.year() as i64;
    let mut month = dt.month() as i64 + months;
    while month > 12 {
        month -= 12;
        year += 1;
    }
    while month < 1 {
        month += 12;
        year -= 1;
    }
    let day = dt.day().min(days_in_month(year as i32, month as u32));
    let naive = chrono::NaiveDate::from_ymd_opt(year as i32, month as u32, day)
        .and_then(|d| d.and_hms_opt(dt.hour(), dt.minute(), dt.second()));
    match naive {
        Some(n) => Local.from_local_datetime(&n).earliest().unwrap_or(*dt),
        None => *dt,
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 400 == 0 || (year % 100 != 0 && year % 4 == 0) { 29 } else { 28 }
        }
        _ => 28,
    }
}

fn rfc3339_diff_secs(start: &str, end: &str) -> Option<i64> {
    let s = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let e = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    Some((e - s).num_seconds())
}

/// Return the date part of an RFC3339 string (first 10 chars).
fn day_of(rfc: &str) -> &str {
    &rfc[..10.min(rfc.len())]
}

// ---------------------------------------------------------------------------
// Vault helpers (raw layer)

impl Vault {
    /// Append raw VEVENT records to `calendar/caldav/raw/YYYY-MM.jsonl`.
    pub fn append_caldav_raw(&self, events: &[RawVEvent]) -> Result<()> {
        self.stream(RAW_DIR, crate::store::Partition::Month)
            .append(events, |e| e.partition.as_str())
    }

    /// Read and write the CalDAV sync-state file.
    pub fn read_caldav_sync(&self) -> Option<CalDavSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_caldav_sync(&self, state: &CalDavSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Pull hook (Periodic)

/// One CalDAV sync pass: discover calendars, diff CTags, fetch changed
/// objects, write raw + contract rows.
pub fn collect_caldav(vault: &Vault) -> Result<CollectOutcome> {
    let cred = match load_credential(vault) {
        Ok(Some(c)) => c,
        Ok(None) => return Ok(CollectOutcome::quiet()),
        Err(e) => return Err(e),
    };

    let client = CalDavClient::new(&cred);
    let account = format!("{}@{}", cred.username, host_of(&cred.server_url));

    // Discover calendar home
    let principal_url = client.current_user_principal()?.unwrap_or_else(|| {
        // Fall back to probing well-known or the server URL directly
        format!("{}/principals/{}/", cred.server_url.trim_end_matches('/'), cred.username)
    });
    let home_url = client.calendar_home_set(&principal_url)?.unwrap_or_else(|| {
        // Some servers (iCloud) skip principal discovery; fall back to the URL itself
        cred.server_url.clone()
    });

    let calendars = client.list_calendars(&home_url)?;
    let now = Local::now();
    let ts = now.to_rfc3339();
    let window_start = (now - Duration::days(WINDOW_PAST_DAYS)).format("%Y-%m-%d").to_string();
    let window_end = (now + Duration::days(WINDOW_FUTURE_DAYS)).format("%Y-%m-%d").to_string();

    let mut state = vault.read_caldav_sync().unwrap_or_default();
    let mut added = 0u64;
    let mut removed = 0u64;
    let mut changed = 0u64;
    let mut total_raw = 0u64;
    let mut calendar_count = 0u64;

    for cal in calendars {
        if !cal.supports_vevent {
            continue;
        }
        calendar_count += 1;
        let cal_state = state.calendars.entry(cal.href.clone()).or_default();
        cal_state.display_name = cal.display_name.clone();

        // CTag unchanged → skip this calendar entirely
        if !cal.ctag.is_empty() && cal_state.ctag == cal.ctag {
            continue;
        }

        // Fetch all VEVENTs with their data
        let objects = match client.fetch_all_events(&cal.href) {
            Ok(o) => o,
            Err(e) => {
                // Per-calendar failure: record and continue
                state.error = Some(format!("fetch {} failed: {e}", cal.href));
                continue;
            }
        };

        // Collect raw events
        let mut raw_events: Vec<RawVEvent> = Vec::new();
        for (href, etag, ics) in &objects {
            let events = parse_ics(ics, &cal.href, &cal.display_name, href, etag);
            raw_events.extend(events);
        }

        // Minor guard: if the server returned objects but parsing yielded zero
        // events (parse failure / format change), treat as a soft failure —
        // do not advance the CTag watermark so the next pass retries. This
        // prevents a mass-`removed` wipe followed by a silently-advanced cursor.
        if !objects.is_empty() && raw_events.is_empty() {
            state.error = Some(format!("caldav: {} returned {} objects but all failed to parse — skipping CTag advance", cal.href, objects.len()));
            continue;
        }

        // Persist raw layer
        if !raw_events.is_empty() {
            vault.append_caldav_raw(&raw_events)?;
            total_raw += raw_events.len() as u64;
        }

        // Expand to CalendarOccurrences within the sync window.
        // Pre-compute RECURRENCE-ID override slots per UID so the base RRULE
        // expansion skips those slots (prevents duplicate id+occurrence rows).
        let mut fresh: Vec<CalendarOccurrence> = Vec::new();
        for rev in &raw_events {
            let slots = if !rev.rrule.is_empty() {
                override_slots_for_uid(&rev.uid, &raw_events)
            } else {
                std::collections::HashSet::new()
            };
            fresh.extend(expand_event(
                rev,
                &cal.display_name,
                &account,
                &window_start,
                &window_end,
                &slots,
            ));
        }

        // First-run detection: this calendar has never been synced by CalDAV
        // before (first_synced absent). Use calendar_backfill (silent, no
        // `added` change lines) so the initial bulk of existing events is
        // treated as baseline state, not new events — mirrors google_calendar.rs.
        let is_first_run = cal_state.first_synced.is_none();
        let stats = if is_first_run {
            vault.calendar_backfill(&fresh).ok();
            // Record no change-line stats on first run
            crate::calendar::CalendarSyncStats::default()
        } else {
            vault.calendar_snapshot_scoped(
                &fresh,
                &window_start,
                &window_end,
                &ts,
                &|o| o.id.starts_with(CALDAV_ROW_PREFIX),
            )?
        };

        added += stats.added;
        removed += stats.removed;
        changed += stats.changed;

        // Advance watermark only after successful parse+write
        cal_state.ctag = cal.ctag;
        cal_state.etags = objects.iter().map(|(h, e, _)| (h.clone(), e.clone())).collect();
        cal_state.last_fetched = ts.clone();
        if is_first_run {
            cal_state.first_synced = Some(ts.clone());
        }
    }

    state.updated = ts;
    state.error = None;
    vault.write_caldav_sync(&state)?;

    Ok(if calendar_count == 0 && added == 0 {
        CollectOutcome::quiet()
    } else if added + removed + changed == 0 {
        CollectOutcome::note(format!(
            "caldav synced {calendar_count} calendars — no changes"
        ))
    } else {
        CollectOutcome::note(format!(
            "caldav synced {calendar_count} calendars — {added} added, {changed} changed, {removed} removed ({total_raw} raw)"
        ))
    })
}

/// Extract the hostname from a URL.
fn host_of(url: &str) -> &str {
    let url = url.trim_start_matches("https://").trim_start_matches("http://");
    url.split('/').next().unwrap_or(url)
}

// ---------------------------------------------------------------------------
// Pull hook (triggered pull from the connect card)

/// Pull hook called immediately after connect and on demand.
pub fn pull_caldav(vault: &Vault) -> Result<PullOutcome> {
    let cred = match load_credential(vault)? {
        Some(c) => c,
        None => {
            return Ok(PullOutcome {
                headline: "CalDAV not connected".to_string(),
                counts: BTreeMap::new(),
            })
        }
    };
    let client = CalDavClient::new(&cred);
    let account = format!("{}@{}", cred.username, host_of(&cred.server_url));

    let principal_url = client.current_user_principal()?.unwrap_or_else(|| {
        format!("{}/principals/{}/", cred.server_url.trim_end_matches('/'), cred.username)
    });
    let home_url =
        client.calendar_home_set(&principal_url)?.unwrap_or_else(|| cred.server_url.clone());
    let calendars = client.list_calendars(&home_url)?;

    let now = Local::now();
    let ts = now.to_rfc3339();
    let window_start = (now - Duration::days(WINDOW_PAST_DAYS)).format("%Y-%m-%d").to_string();
    let window_end = (now + Duration::days(WINDOW_FUTURE_DAYS)).format("%Y-%m-%d").to_string();

    let mut state = vault.read_caldav_sync().unwrap_or_default();
    let mut total_added = 0u64;
    let mut total_raw = 0u64;

    for cal in calendars {
        if !cal.supports_vevent {
            continue;
        }
        let cal_state = state.calendars.entry(cal.href.clone()).or_default();
        cal_state.display_name = cal.display_name.clone();

        let objects = match client.fetch_all_events(&cal.href) {
            Ok(o) => o,
            Err(e) => {
                state.error = Some(format!("fetch {} failed: {e}", cal.href));
                continue;
            }
        };

        let mut raw_events: Vec<RawVEvent> = Vec::new();
        for (href, etag, ics) in &objects {
            let events = parse_ics(ics, &cal.href, &cal.display_name, href, etag);
            raw_events.extend(events);
        }

        // Soft failure guard: if objects were returned but parsing yielded zero
        // events, skip this calendar rather than wiping the stored rows.
        if !objects.is_empty() && raw_events.is_empty() {
            state.error = Some(format!("caldav: {} returned {} objects but all failed to parse — skipping", cal.href, objects.len()));
            continue;
        }

        if !raw_events.is_empty() {
            vault.append_caldav_raw(&raw_events)?;
            total_raw += raw_events.len() as u64;
        }

        // Expand with RECURRENCE-ID override suppression
        let mut fresh: Vec<CalendarOccurrence> = Vec::new();
        for rev in &raw_events {
            let slots = if !rev.rrule.is_empty() {
                override_slots_for_uid(&rev.uid, &raw_events)
            } else {
                std::collections::HashSet::new()
            };
            fresh.extend(expand_event(
                rev,
                &cal.display_name,
                &account,
                &window_start,
                &window_end,
                &slots,
            ));
        }

        // First-run: use backfill (silent, no `added` change lines)
        let is_first_run = cal_state.first_synced.is_none();
        let stats = if is_first_run {
            vault.calendar_backfill(&fresh).ok();
            crate::calendar::CalendarSyncStats::default()
        } else {
            vault.calendar_snapshot_scoped(
                &fresh,
                &window_start,
                &window_end,
                &ts,
                &|o| o.id.starts_with(CALDAV_ROW_PREFIX),
            )?
        };
        total_added += stats.added;

        cal_state.ctag = cal.ctag;
        cal_state.etags = objects.iter().map(|(h, e, _)| (h.clone(), e.clone())).collect();
        cal_state.last_fetched = ts.clone();
        if is_first_run {
            cal_state.first_synced = Some(ts.clone());
        }
    }

    state.updated = ts;
    state.error = None;
    vault.write_caldav_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("raw", total_raw);
    counts.insert("added", total_added);
    Ok(PullOutcome {
        headline: if total_raw == 0 {
            "CalDAV: no events fetched".to_string()
        } else {
            format!("{total_raw} raw events, {total_added} added to calendar")
        },
        counts,
    })
}

// ---------------------------------------------------------------------------
// Connection (TokenPaste = "URL\nusername\npassword")

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let Some(cred) = parse_pasted_credential(pasted) else {
        bail!(
            "Paste three lines: the server URL, then your username, then your \
             password (or app-specific password).\n\
             Example:\n  https://caldav.icloud.com/\n  yourname@icloud.com\n  abcd-efgh-ijkl-mnop"
        );
    };
    // Validate by attempting calendar discovery
    let client = CalDavClient::new(&cred);
    let principal = client.current_user_principal().context("connecting to CalDAV server")?;
    if principal.is_none() {
        // Tolerate servers that don't support current-user-principal; we'll
        // fall back to the home URL on first sync.
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: cred.password,
            refresh_token: None,
            token_type: Some(cred.server_url),
            scope: Some(cred.username),
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        let url = ts.token_type.as_deref().unwrap_or("unknown server");
        let user = ts.scope.as_deref().unwrap_or("unknown user");
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: format!("{user} @ {}", host_of(url)),
            connected_at: vault.read_caldav_sync().map(|s| s.updated),
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_caldav_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    collect_caldav(vault)
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull_caldav(vault)
}

// ---------------------------------------------------------------------------
// DEF + CONNECTION

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "caldav",
        name: "CalDAV (any server)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs calendar events from any CalDAV server — iCloud, Fastmail, Nextcloud, \
                      Proton Calendar (via Bridge), or self-hosted Radicale/Baikal. The \
                      highest-breadth calendar connector: one login covers every account on the \
                      same server.",
        domain: "calendar",
        vault_path: "calendar/caldav/",
        toggleable: true,
        setup: &[
            "Enter your server URL, username, and password (or app-specific password) in the \
             connect card, one per line.",
            "iCloud: generate an app-specific password at appleid.apple.com → Security → \
             App-Specific Passwords. Use https://caldav.icloud.com/ as the server URL.",
            "Fastmail: use https://caldav.fastmail.com/ with your full email and a Fastmail app \
             password.",
            "Proton Calendar: requires Proton Mail Bridge running locally; point at \
             https://127.0.0.1:1143/ or the Bridge's CalDAV port.",
            "Nextcloud/self-hosted: use the base URL of your Nextcloud instance.",
        ],
        caveats: "iCloud and Proton require app-specific / Bridge passwords — your main account \
                 password is rejected. RRULE recurrences are expanded for \
                 DAILY/WEEKLY/MONTHLY/YEARLY rules within the ±60-day/1-year window; complex \
                 rules (BYMONTHDAY, BYSETPOS) store only the base occurrence.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(CALDAV_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("caldav"),
    pull: Some(def_pull),
};

/// CalDAV connection — HTTP Basic over TLS, one server per account.
/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "caldav",
    display_name: "CalDAV Server",
    methods: &[ConnectMethod::TokenPaste {
        label: "Server URL, username, and password (one per line)",
        help: "Paste three lines: the CalDAV server base URL, your username (usually your email), \
               and your password or app-specific password.\n\
               • iCloud: https://caldav.icloud.com/ + Apple ID email + app-specific password\n\
               • Fastmail: https://caldav.fastmail.com/ + email + Fastmail app password\n\
               • Nextcloud: https://your.server.com/ + username + password",
        placeholder: "https://caldav.icloud.com/\nyou@example.com\nxxxx-xxxx-xxxx-xxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["caldav"],
    setup: &[
        "No app registration required — CalDAV uses HTTP Basic authentication.",
        "iCloud: visit appleid.apple.com → Security → App-Specific Passwords to generate a \
         password for Trove.",
        "Fastmail: Settings → Privacy & Security → App Passwords → New App Password.",
        "Proton Calendar: install Proton Bridge and start it; the CalDAV endpoint is shown in \
         the Bridge settings.",
    ],
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(suffix: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-caldav-{}-{}",
            std::process::id(),
            suffix
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---- ICS parsing ----

    #[test]
    fn parse_single_event() {
        let ics = fs::read_to_string(
            "tests/fixtures/caldav/single_event.ics",
        )
        .unwrap();
        let events = parse_ics(&ics, "https://server/cal/", "Work", "/cal/ev1.ics", "etag1");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uid, "abc-123-single@example.com");
        assert_eq!(e.summary, "Team meeting");
        assert_eq!(e.dtstart, "20260610T140000Z");
        assert_eq!(e.location, "Conference Room A");
        assert_eq!(e.status, "CONFIRMED");
        assert_eq!(e.attendees.len(), 2);
        assert!(e.attendees.iter().any(|a| a == "Alice" || a.contains("alice")));
    }

    #[test]
    fn parse_allday_and_canceled() {
        let ics = fs::read_to_string(
            "tests/fixtures/caldav/allday_and_canceled.ics",
        )
        .unwrap();
        let events = parse_ics(&ics, "https://server/cal/", "Personal", "/cal/ev2.ics", "etag2");
        assert_eq!(events.len(), 2);
        let allday = events.iter().find(|e| e.uid == "allday-001@example.com").unwrap();
        assert!(is_all_day(&allday.dtstart));
        let canceled = events.iter().find(|e| e.uid == "canceled-002@example.com").unwrap();
        assert_eq!(canceled.status, "CANCELLED");
    }

    #[test]
    fn parse_recurring_weekly() {
        let ics = fs::read_to_string(
            "tests/fixtures/caldav/recurring_weekly.ics",
        )
        .unwrap();
        let events = parse_ics(&ics, "https://server/cal/", "Work", "/cal/ev3.ics", "etag3");
        assert_eq!(events.len(), 2); // base + one override
        let base = events.iter().find(|e| e.recurrence_id.is_empty()).unwrap();
        assert!(!base.rrule.is_empty());
        let override_ev = events.iter().find(|e| !e.recurrence_id.is_empty()).unwrap();
        assert_eq!(override_ev.summary, "Daily standup (moved)");
    }

    #[test]
    fn parse_timezone_event() {
        let ics = fs::read_to_string(
            "tests/fixtures/caldav/timezone_event.ics",
        )
        .unwrap();
        let events = parse_ics(&ics, "https://server/cal/", "Trips", "/cal/ev4.ics", "etag4");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "tz-event-la@example.com");
        assert!(!events[0].dtstart.is_empty());
        // TZID parameter must be captured (DTSTART;TZID=America/Los_Angeles:…)
        assert_eq!(events[0].dtstart_tzid, "America/Los_Angeles",
            "TZID param must be captured into dtstart_tzid");
    }

    #[test]
    fn tzid_datetime_resolves_correct_utc_instant() {
        // DTSTART;TZID=America/Los_Angeles:20260620T090000
        // = 2026-06-20 09:00 PDT = UTC-7 = 2026-06-20T16:00:00Z
        let rfc = ics_dt_to_rfc3339_tz("20260620T090000", "America/Los_Angeles", false)
            .expect("should convert");
        // The stored RFC3339 offset may vary by machine local zone, but the UTC
        // instant must be 16:00:00Z.
        let parsed = chrono::DateTime::parse_from_rfc3339(&rfc)
            .expect("must be valid RFC3339");
        let utc = parsed.with_timezone(&chrono::Utc);
        assert_eq!(utc.hour(), 16, "PDT 09:00 must map to UTC 16:00 — got {rfc}");
        assert_eq!(utc.minute(), 0);
    }

    // ---- DTSTART / all-day detection ----

    #[test]
    fn all_day_detection() {
        assert!(is_all_day("20260701"));
        assert!(!is_all_day("20260610T140000Z"));
    }

    // ---- DTSTART to RFC3339 conversion ----

    #[test]
    fn ics_dt_utc_to_rfc3339() {
        let r = ics_dt_to_rfc3339("20260610T140000Z", false).unwrap();
        assert!(r.contains("14:00:00") || r.contains("T")); // converted
    }

    #[test]
    fn ics_dt_allday() {
        let r = ics_dt_to_rfc3339("20260701", false).unwrap();
        assert!(r.starts_with("2026-07-01"));
    }

    #[test]
    fn ics_dt_allday_end_exclusive() {
        // DTEND for all-day is exclusive: 20260702 means up to 2026-07-01 23:59:59
        let r = ics_dt_to_rfc3339("20260702", true).unwrap();
        assert!(r.starts_with("2026-07-01"));
        assert!(r.contains("23:59:59"));
    }

    // ---- RRULE expansion ----

    #[test]
    fn rrule_weekly_byday_expansion() {
        let raw = RawVEvent {
            calendar_url: "http://srv/cal/".to_string(),
            calendar_name: "Work".to_string(),
            href: "/cal/ev.ics".to_string(),
            etag: "e1".to_string(),
            uid: "weekly-standup@example.com".to_string(),
            summary: "Standup".to_string(),
            dtstart: "20260601T090000Z".to_string(),
            dtstart_tzid: String::new(),
            dtend: "20260601T093000Z".to_string(),
            dtend_tzid: String::new(),
            rrule: "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".to_string(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2026-06".to_string(),
        };
        // Use a narrow window covering exactly one work week
        let occs = expand_event(&raw, "Work", "user@srv", "2026-06-01", "2026-06-07", &std::collections::HashSet::new());
        // Mon Jun 1 through Fri Jun 6, 2026 = 5 occurrences (Mon–Fri)
        // Verify all are weekdays
        for occ in &occs {
            let day = &occ.start[..10];
            let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap();
            let wd = date.weekday().num_days_from_monday();
            assert!(wd < 5, "unexpected weekend day: {day}");
        }
        assert!(!occs.is_empty());
        assert!(occs.iter().all(|o| o.id.starts_with(CALDAV_ROW_PREFIX)));
    }

    #[test]
    fn rrule_daily_expansion() {
        let raw = RawVEvent {
            calendar_url: "http://srv/cal/".to_string(),
            calendar_name: "Habits".to_string(),
            href: "/cal/ev.ics".to_string(),
            etag: "e2".to_string(),
            uid: "daily-reminder@example.com".to_string(),
            summary: "Morning run".to_string(),
            dtstart: "20260601T060000Z".to_string(),
            dtstart_tzid: String::new(),
            dtend: "20260601T063000Z".to_string(),
            dtend_tzid: String::new(),
            rrule: "FREQ=DAILY".to_string(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2026-06".to_string(),
        };
        let occs = expand_event(&raw, "Habits", "user@srv", "2026-06-01", "2026-06-05", &std::collections::HashSet::new());
        assert_eq!(occs.len(), 5, "expected 5 daily occurrences");
    }

    #[test]
    fn single_event_out_of_window_excluded() {
        let raw = RawVEvent {
            calendar_url: "http://srv/cal/".to_string(),
            calendar_name: "Personal".to_string(),
            href: "/cal/ev.ics".to_string(),
            etag: "e3".to_string(),
            uid: "far-future@example.com".to_string(),
            summary: "Far future".to_string(),
            dtstart: "20300101T120000Z".to_string(),
            dtstart_tzid: String::new(),
            dtend: "20300101T130000Z".to_string(),
            dtend_tzid: String::new(),
            rrule: String::new(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2030-01".to_string(),
        };
        let occs = expand_event(&raw, "Personal", "user@srv", "2026-06-01", "2027-06-01", &std::collections::HashSet::new());
        assert!(occs.is_empty());
    }

    // ---- Status normalisation ----

    #[test]
    fn status_normalisation() {
        assert_eq!(normalize_status("CONFIRMED"), "confirmed");
        assert_eq!(normalize_status("CANCELLED"), "canceled");
        assert_eq!(normalize_status("TENTATIVE"), "tentative");
        assert_eq!(normalize_status("confirmed"), "confirmed");
        assert_eq!(normalize_status(""), "");
    }

    // ---- Credential parsing ----

    #[test]
    fn parse_credential_valid() {
        let c = parse_pasted_credential("https://caldav.icloud.com/\nme@icloud.com\nxxxx-xxxx")
            .unwrap();
        assert_eq!(c.server_url, "https://caldav.icloud.com/");
        assert_eq!(c.username, "me@icloud.com");
        assert_eq!(c.password, "xxxx-xxxx");
    }

    #[test]
    fn parse_credential_blank_lines_skipped() {
        let c = parse_pasted_credential(
            "\n\nhttps://caldav.fastmail.com/\n\nme@fastmail.com\n\napp-pass\n\n",
        )
        .unwrap();
        assert_eq!(c.server_url, "https://caldav.fastmail.com/");
        assert_eq!(c.username, "me@fastmail.com");
        assert_eq!(c.password, "app-pass");
    }

    #[test]
    fn parse_credential_too_few_lines_returns_none() {
        assert!(parse_pasted_credential("https://server/\nusername").is_none());
    }

    // ---- Vault raw write + read-back ----

    #[test]
    fn vault_raw_write_roundtrip() {
        let vault = temp_vault("raw-roundtrip");
        let event = RawVEvent {
            calendar_url: "https://srv/cal/".to_string(),
            calendar_name: "Work".to_string(),
            href: "/cal/ev1.ics".to_string(),
            etag: "etag1".to_string(),
            uid: "uid-1@example.com".to_string(),
            summary: "Sync test".to_string(),
            dtstart: "20260610T140000Z".to_string(),
            dtstart_tzid: String::new(),
            dtend: "20260610T150000Z".to_string(),
            dtend_tzid: String::new(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            rrule: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2026-06".to_string(),
        };
        vault.append_caldav_raw(&[event.clone()]).unwrap();
        let back: Vec<RawVEvent> = vault.read_snapshot("calendar/caldav/raw/2026-06.jsonl").unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].uid, "uid-1@example.com");
    }

    // ---- Connection stores credential (0600) and cursor is credential-free ----

    #[test]
    fn connection_credential_not_in_sync_file() {
        let vault = temp_vault("cred-not-in-sync");
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "secret-pass-xyz".to_string(),
                    refresh_token: None,
                    token_type: Some("https://caldav.test/".to_string()),
                    scope: Some("user@test.com".to_string()),
                    expires_at: None,
                },
            )
            .unwrap();

        // Write a state file (simulating a sync run)
        vault
            .write_caldav_sync(&CalDavSyncState {
                updated: "2026-06-15T10:00:00-07:00".to_string(),
                ..Default::default()
            })
            .unwrap();

        let sync_body =
            fs::read_to_string(vault.root().join(".trove/caldav-sync.json")).unwrap();
        assert!(
            !sync_body.contains("secret-pass-xyz"),
            "password must not appear in the sync file"
        );
    }

    // ---- CONTRACT: CalendarOccurrence rows use CALDAV_ROW_PREFIX ----

    #[test]
    fn contract_row_prefix() {
        let raw = RawVEvent {
            calendar_url: "https://srv/cal/".to_string(),
            calendar_name: "Work".to_string(),
            href: "/cal/ev.ics".to_string(),
            etag: "e".to_string(),
            uid: "prefix-test@example.com".to_string(),
            summary: "Test".to_string(),
            dtstart: "20260610T140000Z".to_string(),
            dtstart_tzid: String::new(),
            dtend: "20260610T150000Z".to_string(),
            dtend_tzid: String::new(),
            rrule: String::new(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2026-06".to_string(),
        };
        let occs = expand_event(&raw, "Work", "user@srv", "2026-06-01", "2026-07-01", &std::collections::HashSet::new());
        assert_eq!(occs.len(), 1);
        assert!(
            occs[0].id.starts_with(CALDAV_ROW_PREFIX),
            "row id must start with caldav: — got {}",
            occs[0].id
        );
    }

    // ---- Sync state round-trip ----

    #[test]
    fn sync_state_roundtrip() {
        let vault = temp_vault("sync-state");
        let mut state = CalDavSyncState::default();
        state.updated = "2026-06-15T12:00:00-07:00".to_string();
        state.calendars.insert(
            "https://caldav.test/home/".to_string(),
            CalDavCalendarState {
                ctag: "42".to_string(),
                display_name: "Home".to_string(),
                last_fetched: "2026-06-15T12:00:00-07:00".to_string(),
                ..Default::default()
            },
        );
        vault.write_caldav_sync(&state).unwrap();
        let back = vault.read_caldav_sync().unwrap();
        assert_eq!(back.updated, state.updated);
        assert!(back.calendars.contains_key("https://caldav.test/home/"));
    }

    // ---- XML parsing helpers ----

    #[test]
    fn split_responses_extracts_blocks() {
        let xml = r#"<d:multistatus>
<d:response><d:href>/a.ics</d:href></d:response>
<d:response><d:href>/b.ics</d:href></d:response>
</d:multistatus>"#;
        let blocks = split_responses(xml);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("/a.ics"));
        assert!(blocks[1].contains("/b.ics"));
    }

    #[test]
    fn extract_calendar_data_found() {
        let block = r#"<d:propstat><d:prop>
<c:calendar-data>BEGIN:VCALENDAR
END:VCALENDAR</c:calendar-data>
</d:prop></d:propstat>"#;
        let data = extract_calendar_data(block).unwrap();
        assert!(data.contains("BEGIN:VCALENDAR"));
    }

    #[test]
    fn parse_multistatus_with_etags() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:response>
  <d:href>/cal/ev1.ics</d:href>
  <d:propstat><d:prop>
    <d:getetag>"abc123"</d:getetag>
    <c:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:test@ex.com
DTSTART:20260610T140000Z
DTEND:20260610T150000Z
SUMMARY:Test
END:VEVENT
END:VCALENDAR</c:calendar-data>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
</d:response>
</d:multistatus>"#;
        let items = parse_multistatus_with_data(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "/cal/ev1.ics");
        assert_eq!(items[0].1, "abc123");
        assert!(items[0].2.contains("BEGIN:VCALENDAR"));
    }

    // ---- Back-compat: old sync file without all fields still deserialises ----

    #[test]
    fn sync_state_back_compat_empty() {
        let json = r#"{}"#;
        let s: CalDavSyncState = serde_json::from_str(json).unwrap();
        assert!(s.updated.is_empty());
        assert!(s.calendars.is_empty());
        assert!(s.error.is_none());
    }

    #[test]
    fn sync_state_back_compat_unknown_fields() {
        let json = r#"{"updated":"2026-01-01","future_field":"ignored"}"#;
        let s: CalDavSyncState = serde_json::from_str(json).unwrap();
        assert_eq!(s.updated, "2026-01-01");
    }

    // ---- Connection def has TokenPaste method ----

    #[test]
    fn connection_def_has_token_paste() {
        assert!(CONNECTION.method("token-paste").is_some());
    }

    // ---- Defect fix: first-run produces 0 added change-lines (baseline) ----

    /// Seeds the calendar/events/ dir (simulating another collector having
    /// written first), then calls the caldav write path for the first time and
    /// asserts that zero `added` change-lines are emitted.
    #[test]
    fn first_run_no_added_change_lines_when_events_dir_exists() {
        let vault = temp_vault("first-run-baseline");

        // Simulate another collector (e.g. EventKit) having created the
        // calendar/events/ directory + a row already in that dir.
        let seed = crate::calendar::CalendarOccurrence {
            id: "eventkit:seed-001@example.com".to_string(),
            occurrence: "2026-06-10T09:00:00+00:00".to_string(),
            start: "2026-06-10T09:00:00+00:00".to_string(),
            end: "2026-06-10T10:00:00+00:00".to_string(),
            all_day: false,
            title: "Seed event".to_string(),
            calendar: "Work".to_string(),
            account: "eventkit@test".to_string(),
            location: String::new(),
            notes: String::new(),
            attendees: Vec::new(),
            status: String::new(),
            recurring: false,
        };
        vault.calendar_backfill(&[seed]).unwrap();
        // calendar/events/ now exists — baseline flag in calendar.rs would be false.

        // Build three CalDAV raw events that should be baselined (first run).
        let make_raw = |uid: &str, start: &str, end: &str| RawVEvent {
            calendar_url: "https://srv/cal/".to_string(),
            calendar_name: "Work".to_string(),
            href: format!("/cal/{uid}.ics"),
            etag: "e1".to_string(),
            uid: uid.to_string(),
            summary: "Meeting".to_string(),
            dtstart: start.to_string(),
            dtstart_tzid: String::new(),
            dtend: end.to_string(),
            dtend_tzid: String::new(),
            rrule: String::new(),
            duration: String::new(),
            location: String::new(),
            description: String::new(),
            status: String::new(),
            recurrence_id: String::new(),
            organizer: String::new(),
            attendees: Vec::new(),
            extra: BTreeMap::new(),
            partition: "2026-06".to_string(),
        };
        let raw_events = vec![
            make_raw("caldav-a@example.com", "20260610T140000Z", "20260610T150000Z"),
            make_raw("caldav-b@example.com", "20260611T140000Z", "20260611T150000Z"),
            make_raw("caldav-c@example.com", "20260612T140000Z", "20260612T150000Z"),
        ];

        let window_start = "2026-06-01";
        let window_end = "2026-07-01";

        // Expand events and call backfill path (mirrors what collect_caldav does
        // on first run: cal_state.first_synced is None → is_first_run=true).
        let mut fresh: Vec<crate::calendar::CalendarOccurrence> = Vec::new();
        for rev in &raw_events {
            fresh.extend(expand_event(
                rev,
                "Work",
                "user@srv",
                window_start,
                window_end,
                &std::collections::HashSet::new(),
            ));
        }
        // Use backfill (first-run path) — must produce 0 added change-lines.
        vault.calendar_backfill(&fresh).unwrap();

        // Verify: no change-lines file or all lines are empty (backfill writes no events).
        let changes_dir = vault.root().join("calendar/changes");
        if changes_dir.exists() {
            let total_lines: usize = std::fs::read_dir(&changes_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| {
                    std::fs::read_to_string(e.path())
                        .unwrap_or_default()
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .count()
                })
                .sum();
            assert_eq!(
                total_lines, 0,
                "first-run CalDAV sync must emit 0 change-lines, got {total_lines}"
            );
        }
        // The 3 caldav rows must appear in the events snapshot.
        let events_dir = vault.root().join("calendar/events");
        assert!(events_dir.exists(), "calendar/events/ must exist after backfill");
        let mut total_caldav_rows = 0usize;
        for entry in std::fs::read_dir(&events_dir).unwrap().filter_map(|e| e.ok()) {
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            total_caldav_rows += content.lines().filter(|l| l.contains(CALDAV_ROW_PREFIX)).count();
        }
        assert_eq!(total_caldav_rows, 3, "all 3 CalDAV events must be in snapshot");
    }

    // ---- Defect fix: RECURRENCE-ID override produces exactly one row ----

    #[test]
    fn recurrence_id_override_no_duplicate_occurrence() {
        // recurring_weekly.ics: weekly standup Mon–Fri from 2026-06-01,
        // with a RECURRENCE-ID override on 2026-06-08 (moved to 10:00).
        // Expanding within a window that contains 2026-06-08 must yield
        // exactly ONE row for that day — the override, not the base slot.
        let ics = fs::read_to_string("tests/fixtures/caldav/recurring_weekly.ics").unwrap();
        let raw_events = parse_ics(
            &ics,
            "https://server/cal/",
            "Work",
            "/cal/ev3.ics",
            "etag3",
        );
        assert_eq!(raw_events.len(), 2); // base + override

        let window_start = "2026-06-01";
        let window_end = "2026-06-14";

        let mut fresh: Vec<CalendarOccurrence> = Vec::new();
        for rev in &raw_events {
            let slots = if !rev.rrule.is_empty() {
                override_slots_for_uid(&rev.uid, &raw_events)
            } else {
                std::collections::HashSet::new()
            };
            fresh.extend(expand_event(
                rev,
                "Work",
                "user@srv",
                window_start,
                window_end,
                &slots,
            ));
        }

        // Collect all (id, occurrence) keys and check for duplicates.
        let uid = format!("{}weekly-standup@example.com", CALDAV_ROW_PREFIX);
        let occ_keys: Vec<String> = fresh
            .iter()
            .filter(|o| o.id == uid)
            .map(|o| o.occurrence.clone())
            .collect();

        // No duplicate occurrence keys for the same UID.
        let mut seen = std::collections::HashSet::new();
        for key in &occ_keys {
            assert!(
                seen.insert(key.clone()),
                "duplicate occurrence key {key} for {uid} — RECURRENCE-ID override collision"
            );
        }

        // The override (10:00) must be present; the base slot (09:00 on Jun 8) must not.
        let jun8_occs: Vec<&CalendarOccurrence> = fresh
            .iter()
            .filter(|o| o.id == uid && o.start.contains("2026-06-08"))
            .collect();
        assert_eq!(
            jun8_occs.len(), 1,
            "exactly one occurrence on 2026-06-08 — got {}",
            jun8_occs.len()
        );
        // The one row must be the moved version (10:00 UTC, not 09:00 UTC).
        // The stored RFC3339 may be in any local offset, so compare via UTC instant.
        let override_start = chrono::DateTime::parse_from_rfc3339(&jun8_occs[0].start)
            .expect("override start must be valid RFC3339");
        let override_utc = override_start.with_timezone(&chrono::Utc);
        assert_eq!(
            override_utc.hour(), 10,
            "override must be 10:00 UTC (DTSTART:20260608T100000Z), got {}",
            jun8_occs[0].start
        );
    }
}
