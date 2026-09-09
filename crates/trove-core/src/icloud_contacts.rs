//! iCloud Contacts via CardDAV — cloud sync with an Apple ID app-specific password.
//!
//! Brief: docs/integrations/icloud-contacts.md
//!
//! Polls `contacts.icloud.com` over CardDAV (RFC 6352) using HTTP Basic auth
//! with an Apple ID + app-specific password. The server URL is hardcoded —
//! this def targets iCloud specifically, not generic CardDAV (that is
//! [`crate::caldav`]). Read-only.
//!
//! **Auth.** Apple's standard 2FA does not work over DAV; the user must
//! generate an app-specific password at appleid.apple.com → Security. The
//! connection TokenPaste accepts two lines:
//!   1. Apple ID (email address)
//!   2. App-specific password (xxxx-xxxx-xxxx-xxxx)
//!
//! **Vault target.** `contacts/icloud-contacts/contacts.jsonl` — snapshot
//! ([`crate::vault::Vault::write_snapshot`]), one [`crate::contacts::Contact`]
//! per line, sorted by UID. Re-syncs are idempotent: contacts are keyed by
//! vCard UID (or name+email composite when absent).
//!
//! **Discovery flow.**
//!   1. PROPFIND `contacts.icloud.com` → current-user-principal URL.
//!   2. PROPFIND principal → addressbook-home-set URL.
//!   3. PROPFIND home with `Depth: 1` → list addressbook collections.
//!   4. REPORT each collection with `addressbook-query` → all vCards.
//!   5. Parse each vCard block → [`crate::contacts::Contact`].
//!   6. Sort by `id`, diff against existing snapshot, atomically rewrite on
//!      change.
//!
//! **Overlap note.** For users with iCloud Contacts sync enabled on macOS,
//! [`crate::apple_contacts`] already federates iCloud contacts via
//! CNContactStore — this def is the fallback for users who bypass that TCC
//! path. The hub setup copy steers users toward apple-contacts.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::{Map, Value};

use crate::contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SOURCE: &str = "icloud-contacts";
const SERVICE: &str = "icloud-contacts";

/// Snapshot path for a given Apple ID account.  When `apple_id` is non-empty
/// the path is partitioned by account (matching the google-contacts convention
/// and the contacts domain schema's `account` field).  Falls back to the
/// unpartitioned path so that the fixed `SNAPSHOT_REL` used by `def_last_data`
/// still finds an existing unpartitioned file during an upgrade.
fn snapshot_rel(apple_id: &str) -> String {
    if apple_id.is_empty() {
        // Fallback for tests / legacy callers without a known Apple ID.
        "contacts/icloud-contacts/contacts.jsonl".to_string()
    } else {
        format!("contacts/icloud-contacts/{apple_id}.jsonl")
    }
}

/// Path used by `def_last_data` to probe whether any snapshot file exists.
/// Glob-like: checks the partitioned path first, then the legacy unpartitioned
/// path.  Kept simple — just returns the first match.
const SNAPSHOT_REL_LEGACY: &str = "contacts/icloud-contacts/contacts.jsonl";
/// iCloud CardDAV server (hardcoded — no user config needed).
const SERVER_URL: &str = "https://contacts.icloud.com";
/// 6 hours between passes — address books churn slowly; matches apple-contacts cadence.
pub const ICLOUD_CONTACTS_SYNC_SECS: u64 = 6 * 3600;

// ---------------------------------------------------------------------------
// Credential helpers

struct Credential {
    username: String,
    password: String,
}

/// Parse a pasted credential of the form:
/// ```text
/// yourname@icloud.com
/// xxxx-xxxx-xxxx-xxxx
/// ```
/// Lines are trimmed; blank lines are skipped. Returns `None` when fewer than
/// 2 non-empty lines are present.
fn parse_pasted_credential(pasted: &str) -> Option<Credential> {
    let parts: Vec<&str> =
        pasted.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    Some(Credential {
        username: parts[0].to_string(),
        password: parts[1].to_string(),
    })
}

fn load_credential(vault: &Vault) -> Result<Option<Credential>> {
    let Some(ts) = vault.load_sync_token(SERVICE)? else {
        return Ok(None);
    };
    let username = ts.scope.unwrap_or_default();
    let password = ts.access_token;
    if username.is_empty() || password.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credential { username, password }))
}

// ---------------------------------------------------------------------------
// CardDAV HTTP client (ureq + HTTP Basic over TLS)

struct CardDavClient {
    server_url: String,
    username: String,
    password: String,
}

impl CardDavClient {
    fn new(cred: &Credential) -> Self {
        let url = SERVER_URL.trim_end_matches('/').to_string() + "/";
        Self {
            server_url: url,
            username: cred.username.clone(),
            password: cred.password.clone(),
        }
    }

    /// Build a ureq request with HTTP Basic auth and CardDAV content type.
    fn req(&self, method: &str, url: &str) -> ureq::Request {
        ureq::request(method, url)
            .set("Authorization", &basic_auth(&self.username, &self.password))
            .set("Content-Type", "application/xml; charset=utf-8")
    }

    /// PROPFIND the iCloud root to get the current-user-principal URL.
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

    /// PROPFIND the principal URL to get the addressbook-home-set URL.
    fn addressbook_home_set(&self, principal_url: &str) -> Result<Option<String>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <card:addressbook-home-set/>
  </d:prop>
</d:propfind>"#;
        let full = to_absolute_url(&self.server_url, principal_url);
        let resp = self
            .req("PROPFIND", &full)
            .set("Depth", "0")
            .send_string(body)
            .context("PROPFIND addressbook-home-set")?;
        let text = resp.into_string()?;
        Ok(extract_dav_href(&text, "addressbook-home-set"))
    }

    /// PROPFIND the home URL with Depth:1 to list addressbook collections.
    fn list_addressbooks(&self, home_url: &str) -> Result<Vec<AddressbookMeta>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"
            xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <cs:getctag/>
  </d:prop>
</d:propfind>"#;
        let full = to_absolute_url(&self.server_url, home_url);
        let resp = self
            .req("PROPFIND", &full)
            .set("Depth", "1")
            .send_string(body)
            .context("PROPFIND addressbook list")?;
        let text = resp.into_string()?;
        Ok(parse_addressbook_list(&text, &self.server_url))
    }

    /// REPORT addressbook-query: fetch all vCards from one addressbook.
    /// Returns a list of `(href, etag, vcard_data)` tuples.
    fn fetch_all_vcards(
        &self,
        addressbook_url: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<card:addressbook-query xmlns:d="DAV:"
                        xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:prop>
    <d:getetag/>
    <card:address-data/>
  </d:prop>
  <card:filter/>
</card:addressbook-query>"#;
        let full = to_absolute_url(&self.server_url, addressbook_url);
        let resp = self
            .req("REPORT", &full)
            .set("Depth", "1")
            .send_string(body)
            .context("REPORT addressbook-query")?;
        let text = resp.into_string()?;
        Ok(parse_multistatus_vcards(&text))
    }
}

/// HTTP Basic auth header value.
fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}")
}

/// Resolve a possibly-relative DAV href to an absolute URL using the server's
/// origin.
fn to_absolute_url(server_url: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    // Extract origin (scheme + host) from server_url.
    let origin = if let Some(rest) = server_url.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or(rest);
        format!("https://{}", host)
    } else if let Some(rest) = server_url.strip_prefix("http://") {
        let host = rest.split('/').next().unwrap_or(rest);
        format!("http://{}", host)
    } else {
        server_url.trim_end_matches('/').to_string()
    };
    format!("{}/{}", origin.trim_end_matches('/'), href.trim_start_matches('/'))
}

// ---------------------------------------------------------------------------
// Minimal XML parsing helpers

/// Extract the first `href` value inside a DAV property element by local name.
/// Returns the trimmed href text, or `None` if not found.
fn extract_dav_href(xml: &str, prop_name: &str) -> Option<String> {
    let lower = xml.to_lowercase();
    let needle = format!(":{}", prop_name.to_lowercase());
    let start = lower.find(&needle)?;
    let after_prop = &xml[start..];
    // Look for <d:href> or <href> within the property block.
    let href_start = after_prop.to_lowercase()
        .find("<d:href>")
        .or_else(|| after_prop.to_lowercase().find("<href>"))?;
    let after_open = {
        let tag_end = after_prop[href_start..].find('>')? + href_start + 1;
        &after_prop[tag_end..]
    };
    let end = after_open.find('<').unwrap_or(after_open.len());
    let href = after_open[..end].trim().to_string();
    if href.is_empty() { None } else { Some(href) }
}

/// An addressbook collection discovered via PROPFIND.
#[derive(Debug, Clone)]
struct AddressbookMeta {
    /// Absolute URL of the collection.
    url: String,
    /// Display name.
    display_name: String,
}

/// Parse the PROPFIND Depth:1 multistatus response to extract addressbook
/// collections. A `<d:response>` that contains the CardDAV addressbook
/// element (`:addressbook/` or `:addressbook>`) in its `<d:resourcetype>`
/// block is an addressbook collection.
fn parse_addressbook_list(xml: &str, server_url: &str) -> Vec<AddressbookMeta> {
    let lower = xml.to_lowercase();
    let mut result = Vec::new();
    let mut pos = 0;

    while let Some(rel) = lower[pos..].find("<d:response") {
        let start = pos + rel;
        let end = lower[start..]
            .find("</d:response>")
            .map(|e| start + e + "</d:response>".len())
            .unwrap_or(xml.len());
        let block = &xml[start..end];
        let block_lower = &lower[start..end];

        // Only include collections that declare the CardDAV addressbook
        // resource type — the element tag must end in `:addressbook` followed
        // by `/` (self-closing) or `>`, not just appear in a URL path.
        // Matches `:addressbook/>`, `:addressbook>`, `<addressbook/>` etc.
        let is_addressbook = block_lower.contains(":addressbook/>")
            || block_lower.contains(":addressbook>")
            || block_lower.contains("<addressbook/>")
            || block_lower.contains("<addressbook>");

        if is_addressbook {
            if let Some(href_pos) = block_lower.find("<d:href>") {
                let after = &block[href_pos + "<d:href>".len()..];
                if let Some(href_end) = after.find("</d:href>") {
                    let href = after[..href_end].trim().to_string();
                    if !href.is_empty() {
                        let name = if let Some(dn) = block_lower.find("<d:displayname>") {
                            let after_dn = &block[dn + "<d:displayname>".len()..];
                            after_dn
                                .find("</d:displayname>")
                                .map(|e| after_dn[..e].trim().to_string())
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };
                        result.push(AddressbookMeta {
                            url: to_absolute_url(server_url, &href),
                            display_name: name,
                        });
                    }
                }
            }
        }
        pos = end;
    }
    result
}

/// Parse a CardDAV multistatus REPORT (addressbook-query) into
/// `Vec<(href, etag, vcard_data)>`. Extracts `address-data` content (any
/// namespace prefix) from each `<d:response>` block.
fn parse_multistatus_vcards(xml: &str) -> Vec<(String, String, String)> {
    let lower = xml.to_lowercase();
    let mut result = Vec::new();
    let mut pos = 0;

    while let Some(rel) = lower[pos..].find("<d:response") {
        let start = pos + rel;
        let end = lower[start..]
            .find("</d:response>")
            .map(|e| start + e + "</d:response>".len())
            .unwrap_or(xml.len());
        let block = &xml[start..end];
        let block_lower = &lower[start..end];

        // href
        let href = if let Some(h) = block_lower.find("<d:href>") {
            let after = &block[h + "<d:href>".len()..];
            after.find("</d:href>").map(|e| after[..e].trim().to_string()).unwrap_or_default()
        } else {
            pos = end;
            continue;
        };

        // etag (any prefix → search for `getetag>`)
        let etag = extract_element_value(block, "getetag").unwrap_or_default();

        // address-data vCard text (any prefix → search for `address-data>`).
        // XML entities (&amp; &lt; &gt; &quot; &apos;) are mandatory within
        // element text content; unescape before handing off to the vCard parser.
        let vcard = extract_element_value(block, "address-data")
            .map(|s| xml_unescape(&s))
            .unwrap_or_default();

        if !href.is_empty() && !vcard.is_empty() {
            result.push((href, etag, vcard));
        }
        pos = end;
    }
    result
}

/// Extract the text content of the first XML element whose local name (the
/// part after `:`) matches `local_name` (case-insensitive).
fn extract_element_value(xml: &str, local_name: &str) -> Option<String> {
    let lower = xml.to_lowercase();
    let local_lower = local_name.to_lowercase();
    // Find the opening tag: `:<local_name>` (after namespace prefix).
    let open_needle = format!(":{}>" , local_lower);
    let open = lower.find(&open_needle)?;
    let content_start = open + open_needle.len();
    let after = &xml[content_start..];
    let after_lower = &lower[content_start..];
    // Find the closing tag: `:<local_name>` preceded by `</`.
    let close_needle = format!(":{}>", local_lower);
    let close_rel = after_lower.find(&close_needle)?;
    // Walk back to find `</` before the closing tag.
    let before_close = &after_lower[..close_rel];
    let slash_rel = before_close.rfind("</")?;
    Some(after[..slash_rel].to_string())
}

/// Unescape the five predefined XML entity references that are mandatory
/// within element text content.  CardDAV delivers vCard data inside an
/// `<address-data>` XML element, so `&`, `<`, `>`, `"`, and `'` are encoded
/// by the server and must be decoded before feeding the text to the vCard
/// parser.
fn xml_unescape(s: &str) -> String {
    // Fast path: skip allocation when no `&` is present.
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Try each entity reference; fall through to literal `&` if none match.
        if let Some(tail) = rest.strip_prefix("&amp;") {
            out.push('&');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("&lt;") {
            out.push('<');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("&gt;") {
            out.push('>');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("&quot;") {
            out.push('"');
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("&apos;") {
            out.push('\'');
            rest = tail;
        } else {
            // Literal `&` that is not a recognized entity — copy as-is.
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// vCard block parser (inline; RFC 6350 / 3.0 logic without crate dependency)

/// Parse a vCard text block (lines between BEGIN:VCARD / END:VCARD, without
/// those sentinel lines) into a [`Contact`]. Returns `None` for a block with
/// no usable identity.
pub(crate) fn parse_vcard_block(block: &str) -> Option<Contact> {
    let mut uid = String::new();
    let mut fn_ = String::new();
    let mut given = String::new();
    let mut family = String::new();
    let mut emails: Vec<String> = Vec::new();
    let mut phones: Vec<String> = Vec::new();
    let mut orgs: Vec<ContactOrg> = Vec::new();
    let mut photo_url = String::new();
    let mut updated: Option<String> = None;
    let mut extra: Map<String, Value> = Map::new();

    let mut addresses: Vec<Value> = Vec::new();
    let mut urls: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut birthdays: Vec<Value> = Vec::new();
    let mut impp_vals: Vec<String> = Vec::new();

    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Split on first colon — value may itself contain colons.
        let Some((lhs, value)) = line.split_once(':') else {
            continue;
        };
        // Strip group prefix (e.g. `item1.EMAIL` → `EMAIL`).
        let bare_lhs = if let Some(dot) = lhs.rfind('.') { &lhs[dot + 1..] } else { lhs };
        // Property name (before first `;`).
        let prop_name = bare_lhs.split(';').next().unwrap_or(bare_lhs).to_uppercase();
        // Lowercased params string for param extraction.
        let params_str = bare_lhs.to_lowercase();
        // Extra key: lowercased local name.
        let extra_key = prop_name.to_lowercase();

        match prop_name.as_str() {
            "VERSION" | "PRODID" | "BEGIN" | "END" => {}

            "UID" => uid = value.trim().to_string(),

            "FN" => {
                if fn_.is_empty() {
                    fn_ = vcard_decode(value);
                }
            }

            "N" => {
                let parts: Vec<&str> = value.splitn(6, ';').collect();
                if family.is_empty() {
                    family =
                        parts.first().map(|s| vcard_decode(s.trim())).unwrap_or_default();
                }
                if given.is_empty() {
                    given =
                        parts.get(1).map(|s| vcard_decode(s.trim())).unwrap_or_default();
                }
            }

            "EMAIL" => {
                let e = normalize_email(&vcard_decode(value));
                if !e.is_empty() {
                    emails.push(e);
                }
            }

            "TEL" => {
                let raw = vcard_decode(value);
                let p = normalize_phone(raw.trim(), None);
                if !p.is_empty() {
                    phones.push(p);
                }
            }

            "ORG" => {
                let parts: Vec<&str> = value.splitn(3, ';').collect();
                let org_name = parts
                    .first()
                    .map(|s| vcard_decode(s.trim()))
                    .filter(|s| !s.is_empty());
                if org_name.is_some() {
                    orgs.push(ContactOrg { name: org_name, title: None });
                }
            }

            "TITLE" => {
                let t = vcard_decode(value);
                if !t.is_empty() {
                    if let Some(last) = orgs.last_mut() {
                        if last.title.is_none() {
                            last.title = Some(t.clone());
                        }
                    } else {
                        orgs.push(ContactOrg { name: None, title: Some(t) });
                    }
                }
            }

            "PHOTO" => {
                if !is_photo_blob(&params_str, value) {
                    let url = value.trim().to_string();
                    if !url.is_empty() && url.contains("://") {
                        photo_url = url;
                    }
                }
                // Blobs silently dropped.
            }

            "ADR" => {
                let parts: Vec<&str> = value.splitn(8, ';').collect();
                let formatted = parts
                    .iter()
                    .map(|s| vcard_decode(s.trim()))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !formatted.is_empty() {
                    let type_label = extract_type_param(&params_str);
                    let mut obj = Map::new();
                    if !type_label.is_empty() {
                        obj.insert("label".into(), Value::String(type_label));
                    }
                    obj.insert("value".into(), Value::String(formatted));
                    addresses.push(Value::Object(obj));
                }
            }

            "URL" => {
                let url_val = vcard_decode(value.trim());
                if !url_val.is_empty() {
                    let type_label = extract_type_param(&params_str);
                    let mut obj = Map::new();
                    if !type_label.is_empty() {
                        obj.insert("label".into(), Value::String(type_label));
                    }
                    obj.insert("value".into(), Value::String(url_val));
                    urls.push(Value::Object(obj));
                }
            }

            "NOTE" => {
                let n = vcard_decode(value);
                if !n.is_empty() {
                    notes.push(n);
                }
            }

            "BDAY" => {
                let raw = value.trim();
                let apple_omit = params_str.contains("x-apple-omit-year");
                if let Some(bd) = parse_vcard_date(raw) {
                    let bd = if apple_omit {
                        strip_year(bd)
                    } else if let Some(y) = bd
                        .as_object()
                        .and_then(|o| o.get("year"))
                        .and_then(|v| v.as_i64())
                    {
                        if y == 1604 { strip_year(bd) } else { bd }
                    } else {
                        bd
                    };
                    birthdays.push(bd);
                }
            }

            "IMPP" => {
                let v = vcard_decode(value.trim());
                if !v.is_empty() {
                    impp_vals.push(v);
                }
            }

            "REV" => {
                let rev = value.trim();
                if !rev.is_empty() && updated.is_none() {
                    updated = Some(normalize_rev(rev));
                }
            }

            _ => {
                // Unknown properties → extra, accumulating duplicates as arrays.
                let val = Value::String(vcard_decode(value));
                match extra.get_mut(&extra_key) {
                    Some(Value::Array(arr)) => arr.push(val),
                    Some(existing_val) => {
                        let prev = existing_val.clone();
                        *existing_val = Value::Array(vec![prev, val]);
                    }
                    None => {
                        extra.insert(extra_key, val);
                    }
                }
            }
        }
    }

    // Build extras from structured arrays.
    if !addresses.is_empty() {
        extra.insert("addresses".into(), Value::Array(addresses));
    }
    if !urls.is_empty() {
        extra.insert("urls".into(), Value::Array(urls));
    }
    if !notes.is_empty() {
        extra.insert("note".into(), Value::String(notes.join("\n")));
    }
    if !birthdays.is_empty() {
        extra.insert(
            "birthdays".into(),
            Value::Array(
                birthdays
                    .into_iter()
                    .map(|d| {
                        let mut m = Map::new();
                        m.insert("date".into(), d);
                        Value::Object(m)
                    })
                    .collect(),
            ),
        );
    }
    if !impp_vals.is_empty() {
        extra.insert(
            "impp".into(),
            Value::Array(impp_vals.into_iter().map(Value::String).collect()),
        );
    }

    // Dedupe emails/phones (preserve first-seen order after normalization).
    let emails: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        emails.into_iter().filter(|e| seen.insert(e.clone())).collect()
    };
    let phones: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        phones.into_iter().filter(|p| seen.insert(p.clone())).collect()
    };

    // Display name: prefer FN; fall back to given+family.
    let name = if !fn_.trim().is_empty() {
        fn_.trim().to_string()
    } else {
        format!("{} {}", given.trim(), family.trim()).trim().to_string()
    };

    // Stable dedupe key: UID wins; else composite name+email.
    let id = if !uid.trim().is_empty() {
        uid.trim().to_string()
    } else if !name.is_empty() || !emails.is_empty() {
        let email_part = emails.first().map(|s| s.as_str()).unwrap_or("");
        format!("{}\x1e{}", name, email_part)
    } else {
        return None;
    };

    Some(Contact {
        source: SOURCE.to_string(),
        id,
        account: String::new(),
        name,
        given,
        family,
        emails,
        phones,
        orgs,
        photo: photo_url,
        other: false,
        updated,
        extra,
    })
}

/// Determine whether a PHOTO property value is an inline base64 blob.
fn is_photo_blob(params_str: &str, value: &str) -> bool {
    if value.trim_start().starts_with("data:") {
        return true;
    }
    if params_str.contains("encoding=b") || params_str.contains("encoding=base64") {
        return true;
    }
    // Heuristic: long run of base64 chars with no URL pattern.
    value.len() > 200
        && !value.contains("://")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
}

/// Extract a `type=...` param value from a lowercased params string.
fn extract_type_param(params_str: &str) -> String {
    if let Some(pos) = params_str.find("type=") {
        let after = &params_str[pos + "type=".len()..];
        let end = after.find(';').unwrap_or(after.len());
        return after[..end].trim_matches('"').to_string();
    }
    String::new()
}

/// Decode vCard backslash-escape sequences (`\n` → newline, `\,` → `,`,
/// `\\` → `\`, `\;` → `;`).
fn vcard_decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') | Some('N') => { chars.next(); out.push('\n'); }
                Some(',') => { chars.next(); out.push(','); }
                Some(';') => { chars.next(); out.push(';'); }
                Some('\\') => { chars.next(); out.push('\\'); }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Remove the `year` key from a `{year, month, day}` Value::Object.
fn strip_year(mut v: Value) -> Value {
    if let Value::Object(ref mut m) = v {
        m.remove("year");
    }
    v
}

/// Parse a vCard date string into `{month, day[, year]}`.
fn parse_vcard_date(raw: &str) -> Option<Value> {
    let s = raw.trim();
    // Year-less forms begin with `--`.
    if let Some(rest) = s.strip_prefix("--") {
        // `--MMDD` (4 chars, no dash) — vCard 4.0 compact form.
        if rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit()) {
            let month: i32 = rest[..2].parse().ok()?;
            let day: i32 = rest[2..].parse().ok()?;
            let mut m = Map::new();
            m.insert("month".into(), Value::from(month));
            m.insert("day".into(), Value::from(day));
            return Some(Value::Object(m));
        }
        // `--MM-DD` (5 chars with dash) — ISO 8601:2000 truncated dashed form.
        if rest.len() == 5 {
            let parts: Vec<&str> = rest.splitn(2, '-').collect();
            if parts.len() == 2 {
                let month: i32 = parts[0].parse().ok()?;
                let day: i32 = parts[1].parse().ok()?;
                let mut m = Map::new();
                m.insert("month".into(), Value::from(month));
                m.insert("day".into(), Value::from(day));
                return Some(Value::Object(m));
            }
        }
        return None;
    }
    // `YYYYMMDD` — compact form.
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        let year: i32 = s[..4].parse().ok()?;
        let month: i32 = s[4..6].parse().ok()?;
        let day: i32 = s[6..].parse().ok()?;
        let mut m = Map::new();
        m.insert("year".into(), Value::from(year));
        m.insert("month".into(), Value::from(month));
        m.insert("day".into(), Value::from(day));
        return Some(Value::Object(m));
    }
    // `YYYY-MM-DD` — dashed form (includes iCloud's `1604-MM-DD` sentinel).
    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() >= 3 {
            let year: i32 = parts[0].parse().ok()?;
            let month: i32 = parts[1].parse().ok()?;
            let day: i32 = parts[2]
                .trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()?;
            if (1..=12).contains(&month) && (1..=31).contains(&day) {
                let mut m = Map::new();
                m.insert("year".into(), Value::from(year));
                m.insert("month".into(), Value::from(month));
                m.insert("day".into(), Value::from(day));
                return Some(Value::Object(m));
            }
        }
    }
    None
}

/// Normalize a REV timestamp to RFC 3339. Compact ISO (`20240225T020408Z`) →
/// dashed; already-dashed strings pass through.
fn normalize_rev(raw: &str) -> String {
    let s = raw.trim();
    if s.contains('-') {
        return s.to_string();
    }
    if s.len() >= 15 && s.chars().nth(8) == Some('T') {
        let date = &s[..8];
        let time = &s[9..];
        if date.chars().all(|c| c.is_ascii_digit()) && time.len() >= 6 {
            let suffix = if time.ends_with('Z') { "Z" } else { "" };
            let td = &time[..6.min(time.len())];
            if td.chars().all(|c| c.is_ascii_digit()) {
                return format!(
                    "{}-{}-{}T{}:{}:{}{}",
                    &date[..4],
                    &date[4..6],
                    &date[6..8],
                    &td[..2],
                    &td[2..4],
                    &td[4..6],
                    suffix
                );
            }
        }
    }
    s.to_string()
}

/// Unfold RFC-style line continuations in a vCard body (CRLF+SP or LF+SP).
fn unfold_vcard(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut lines = raw.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim_end_matches('\r');
        let mut logical = line.to_string();
        loop {
            match lines.peek() {
                Some(next) if next.starts_with(' ') || next.starts_with('\t') => {
                    let cont = lines.next().unwrap().trim_end_matches('\r');
                    logical.push_str(cont.trim_start());
                }
                _ => break,
            }
        }
        out.push_str(&logical);
        out.push('\n');
    }
    out
}

/// Split a raw vCard collection body (as returned from CardDAV) into
/// individual blocks and parse each one. Returns successfully parsed contacts.
fn parse_vcard_text(text: &str) -> Vec<Contact> {
    let unfolded = unfold_vcard(text);
    let mut contacts = Vec::new();
    let mut in_card = false;
    let mut current: Vec<String> = Vec::new();

    for line in unfolded.lines() {
        let upper = line.trim().to_uppercase();
        if upper == "BEGIN:VCARD" {
            in_card = true;
            current.clear();
        } else if upper == "END:VCARD" {
            if in_card {
                let block = current.join("\n");
                if let Some(contact) = parse_vcard_block(&block) {
                    contacts.push(contact);
                }
                current.clear();
            }
            in_card = false;
        } else if in_card {
            current.push(line.to_string());
        }
    }
    contacts
}

// ---------------------------------------------------------------------------
// Sync stats

/// Statistics from one iCloud Contacts sync pass.
#[derive(Debug, Clone, Default)]
pub struct ICloudContactsSyncStats {
    /// Contacts changed (added, updated, or removed) this pass.
    pub changed: u64,
    /// Total contacts in the snapshot after the pass.
    pub total: u64,
    /// True when no credential is stored — the pass was silently skipped.
    pub skipped_no_credential: bool,
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One iCloud Contacts sync pass. Silently a no-op when no credential is
    /// stored.
    pub fn collect_icloud_contacts(&self) -> Result<ICloudContactsSyncStats> {
        let Some(cred) = load_credential(self)? else {
            return Ok(ICloudContactsSyncStats {
                skipped_no_credential: true,
                ..Default::default()
            });
        };
        let apple_id = cred.username.clone();
        let contacts = fetch_all_icloud_contacts(&cred)?;
        self.apply_icloud_contacts(contacts, &apple_id)
    }

    /// Map a freshly-fetched list and commit it as the snapshot. Split out
    /// so tests can inject contacts directly without a network call.
    ///
    /// `apple_id` — the Apple ID (email) for the authenticated account.  Used
    /// to partition the snapshot path and populate `Contact.account`, matching
    /// the google-contacts convention.  Pass an empty string in tests where no
    /// real Apple ID is available.
    pub(crate) fn apply_icloud_contacts(
        &self,
        mut contacts: Vec<Contact>,
        apple_id: &str,
    ) -> Result<ICloudContactsSyncStats> {
        // Stamp the account field so vault rows are self-describing.
        if !apple_id.is_empty() {
            for c in &mut contacts {
                if c.account.is_empty() {
                    c.account = apple_id.to_string();
                }
            }
        }
        contacts.sort_by(|a, b| a.id.cmp(&b.id));

        let rel = snapshot_rel(apple_id);
        let existing: Vec<Contact> = self.read_snapshot(&rel)?;

        // Guard: if the fetch returned zero contacts but the previous snapshot
        // was non-empty, this almost certainly indicates a parse failure (the
        // hand-rolled XML matchers silently returned nothing against an
        // unexpected server response shape) rather than the user genuinely
        // deleting every contact.  Overwriting the snapshot empty would destroy
        // data; instead, bail with an error so the operator can investigate.
        // A true "user deleted everything" case will only occur when the
        // previous snapshot was already empty or the fetch reported individual
        // vCard deletes via REPORT — not a common real-world event.
        if contacts.is_empty() && !existing.is_empty() {
            bail!(
                "iCloud Contacts sync returned 0 contacts but the previous snapshot has {}. \
                 This likely indicates a parse failure against an unexpected server response \
                 shape. Snapshot left untouched; check network connectivity and server response.",
                existing.len()
            );
        }

        let changed = snapshot_diff_count(&existing, &contacts);
        let total = contacts.len() as u64;
        let exists = self.resolve(&rel).map(|p| p.exists()).unwrap_or(false);
        if changed > 0 || !exists {
            self.write_snapshot(&rel, &contacts)?;
        }
        Ok(ICloudContactsSyncStats { changed, total, skipped_no_credential: false })
    }
}

/// Fetch all contacts from every addressbook on the user's iCloud CardDAV
/// account. Returns a flat list keyed by `id` (last addressbook wins on UID
/// collision — an edge case for shared/delegated books).
fn fetch_all_icloud_contacts(cred: &Credential) -> Result<Vec<Contact>> {
    let client = CardDavClient::new(cred);

    let principal = client
        .current_user_principal()
        .context("discovering iCloud CardDAV principal")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "iCloud CardDAV did not return a current-user-principal; \
                 check your Apple ID and app-specific password"
            )
        })?;

    let home = client
        .addressbook_home_set(&principal)
        .context("fetching addressbook-home-set")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "iCloud did not return an addressbook-home-set — \
                 the account may not have Contacts enabled"
            )
        })?;

    let books = client.list_addressbooks(&home)?;

    let mut seen: std::collections::HashMap<String, Contact> =
        std::collections::HashMap::new();
    for book in &books {
        let vcards = client
            .fetch_all_vcards(&book.url)
            .with_context(|| format!("fetching vCards from {}", book.display_name))?;
        for (_href, _etag, vcard_data) in vcards {
            for contact in parse_vcard_text(&vcard_data) {
                seen.insert(contact.id.clone(), contact);
            }
        }
    }

    Ok(seen.into_values().collect())
}

/// Count contacts that differ between old and new snapshots (adds, removes,
/// and field changes), keyed by `id`.
fn snapshot_diff_count(old: &[Contact], new: &[Contact]) -> u64 {
    let old_by: BTreeMap<&str, &Contact> = old.iter().map(|c| (c.id.as_str(), c)).collect();
    let new_by: BTreeMap<&str, &Contact> = new.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut changed = 0u64;
    for (id, c) in &new_by {
        match old_by.get(id) {
            Some(prev) if *prev == *c => {}
            _ => changed += 1,
        }
    }
    for id in old_by.keys() {
        if !new_by.contains_key(id) {
            changed += 1;
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// DEF + CONNECTION hooks

fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    use crate::registry::CollectOutcome;
    let s = vault.collect_icloud_contacts()?;
    Ok(CollectOutcome::note_if(s.changed > 0, || {
        format!("iCloud contacts synced — {} changes ({} total)", s.changed, s.total)
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    // Try the partitioned path keyed by the stored Apple ID first.
    if let Ok(Some(ts)) = vault.load_sync_token(SERVICE) {
        let apple_id = ts.scope.unwrap_or_default();
        if !apple_id.is_empty() {
            let rel = snapshot_rel(&apple_id);
            if let Ok(path) = vault.resolve(&rel) {
                if path.exists() {
                    return crate::registry::file_mtime(&path);
                }
            }
        }
    }
    // Fallback: legacy unpartitioned path (upgrade compatibility).
    let path = vault.resolve(SNAPSHOT_REL_LEGACY).ok()?;
    crate::registry::file_mtime(&path)
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_icloud_contacts()?;
    let headline = if s.skipped_no_credential {
        "iCloud Contacts not connected — paste Apple ID + app-specific password".to_string()
    } else if s.changed > 0 {
        format!("{} contact changes ({} total)", s.changed, s.total)
    } else {
        format!("contacts up to date ({} total)", s.total)
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("changed", s.changed), ("total", s.total)]),
    })
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let Some(cred) = parse_pasted_credential(pasted) else {
        bail!(
            "Paste two lines: your Apple ID (email) then your app-specific password.\n\
             Example:\n  yourname@icloud.com\n  abcd-efgh-ijkl-mnop\n\n\
             Generate an app-specific password at appleid.apple.com → \
             Sign-In and Security → App-Specific Passwords."
        );
    };
    // Validate by attempting principal discovery — a wrong password 401s.
    let client = CardDavClient::new(&cred);
    let _ = client
        .current_user_principal()
        .context(
            "connecting to iCloud CardDAV \
             (check your Apple ID email and app-specific password)",
        )?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: cred.password,
            refresh_token: None,
            token_type: None,
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
        let user = ts.scope.unwrap_or_default();
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: user,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "icloud-contacts",
        name: "iCloud Contacts (CardDAV)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your iCloud address book over CardDAV using an Apple ID and \
                      app-specific password. For users with macOS Contacts sync enabled, \
                      Apple Contacts (CNContactStore) covers the same data more simply — \
                      use this when Contacts sync is turned off or the TCC grant is bypassed.",
        domain: "contacts",
        vault_path: "contacts/icloud-contacts/",
        toggleable: true,
        setup: &[
            "Visit appleid.apple.com → Sign-In and Security → App-Specific Passwords.",
            "Click '+' to generate a new password and name it 'Trove'.",
            "Paste your Apple ID email on line 1 and the generated password on line 2 in the connect card.",
            "If macOS Contacts sync is on, Apple Contacts already covers this data — no need to connect both.",
        ],
        caveats: "App-specific passwords can be revoked at appleid.apple.com if compromised. \
                  For most users Apple Contacts (CNContactStore) is simpler — it federates \
                  iCloud contacts automatically.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ICLOUD_CONTACTS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("icloud-contacts"),
    pull: Some(def_pull),
};

/// Registered in [`crate::integrations::CONNECTIONS`]. The integrator adds
/// `&crate::icloud_contacts::CONNECTION,` to the CONNECTIONS slice.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "icloud-contacts",
    display_name: "iCloud Contacts",
    methods: &[ConnectMethod::TokenPaste {
        label: "Apple ID and app-specific password (one per line)",
        help: "Paste two lines: your Apple ID email address, then an app-specific password \
               generated at appleid.apple.com → Sign-In and Security → App-Specific Passwords. \
               Your normal Apple ID password does NOT work over CardDAV — Apple's 2FA blocks it.",
        placeholder: "yourname@icloud.com\nxxxx-xxxx-xxxx-xxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["icloud-contacts"],
    setup: &[
        "Go to appleid.apple.com and sign in.",
        "Navigate to Sign-In and Security → App-Specific Passwords.",
        "Click '+', name the password 'Trove', and copy the generated password.",
        "Paste your Apple ID email on the first line and the app password on the second line in the connect card.",
    ],
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-icloud-contacts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixture vCard blocks (community-verified iCloud shapes)

    // Typical iCloud vCard 3.0: X-APPLE-OMIT-YEAR birthday, UID, all main fields.
    const VCARD3_ICLOUD: &str = "\
UID:12345678-1234-5678-abcd-1234567890ab
FN:Jane Icloud
N:Icloud;Jane;;;
EMAIL;type=INTERNET;type=pref:jane@icloud.com
TEL;type=CELL:+1 (415) 555-0100
ORG:Example Inc;
TITLE:Engineer
ADR;type=HOME:;;42 Apple Lane;Cupertino;CA;95014;USA
URL:https://jane.example.com
NOTE:Colleague at work.
BDAY;X-APPLE-OMIT-YEAR=1604:1604-07-04
REV:2024-01-15T10:00:00Z
";

    // vCard 3.0 with no UID → composite id from name+email.
    const VCARD3_NO_UID: &str = "\
FN:Bob NoUid
N:NoUid;Bob;;;
EMAIL:bob@example.com
TEL:+14155550001
";

    // vCard 4.0: UID, year-less birthday (--MMDD), IMPP, REV in compact form.
    const VCARD4_FULL: &str = "\
UID:urn:uuid:deadbeef-dead-beef-dead-beefdeadbeef
FN:Carol Vcard4
N:Vcard4;Carol;;;
EMAIL;TYPE=work:carol@example.com
TEL;TYPE=cell:+14155550002
BDAY:--0314
IMPP:xmpp:carol@jabber.example
REV:20240225T020408Z
";

    // vCard with inline base64 PHOTO blob — must be stripped.
    const VCARD3_PHOTO_BLOB: &str = "\
UID:photo-blob-test
FN:Photo Blob Test
PHOTO;ENCODING=b;TYPE=image/png:iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==
EMAIL:pt@example.com
";

    // vCard 4.0 with real photo URL — should be kept.
    const VCARD4_PHOTO_URL: &str = "\
UID:photo-url-test
FN:Photo Url Test
PHOTO:https://example.com/jane.jpg
EMAIL:pu@example.com
";

    // ---------------------------------------------------------------------------
    // parse_vcard_block tests

    #[test]
    fn icloud_vcard3_maps_all_fields() {
        let c = parse_vcard_block(VCARD3_ICLOUD).expect("should parse");
        assert_eq!(c.source, "icloud-contacts");
        assert_eq!(c.id, "12345678-1234-5678-abcd-1234567890ab");
        assert_eq!(c.name, "Jane Icloud");
        assert_eq!(c.given, "Jane");
        assert_eq!(c.family, "Icloud");
        assert_eq!(c.emails, vec!["jane@icloud.com"]);
        assert_eq!(c.phones, vec!["+14155550100"]);
        assert_eq!(c.orgs.len(), 1);
        assert_eq!(c.orgs[0].name.as_deref(), Some("Example Inc"));
        assert_eq!(c.orgs[0].title.as_deref(), Some("Engineer"));
        assert!(c.extra.contains_key("addresses"), "addresses in extra");
        assert!(c.extra.contains_key("urls"), "urls in extra");
        let note = c.extra.get("note").and_then(Value::as_str).unwrap_or("");
        assert!(note.contains("Colleague"), "note present: {note:?}");
        // X-APPLE-OMIT-YEAR birthday → no year key.
        let bdays = c.extra["birthdays"].as_array().unwrap();
        assert_eq!(bdays[0]["date"]["month"], 7);
        assert_eq!(bdays[0]["date"]["day"], 4);
        assert!(bdays[0]["date"].get("year").is_none(), "year must be absent for omit-year");
        assert_eq!(c.updated.as_deref(), Some("2024-01-15T10:00:00Z"));
    }

    #[test]
    fn no_uid_contact_gets_composite_id() {
        let c = parse_vcard_block(VCARD3_NO_UID).expect("should parse");
        assert!(c.id.contains("Bob NoUid"), "composite id: {}", c.id);
        assert_eq!(c.emails, vec!["bob@example.com"]);
    }

    #[test]
    fn vcard4_yearless_bday_and_impp() {
        let c = parse_vcard_block(VCARD4_FULL).expect("should parse");
        assert_eq!(c.id, "urn:uuid:deadbeef-dead-beef-dead-beefdeadbeef");
        // --0314 → month=3, day=14, no year.
        let bdays = c.extra["birthdays"].as_array().unwrap();
        assert_eq!(bdays[0]["date"]["month"], 3);
        assert_eq!(bdays[0]["date"]["day"], 14);
        assert!(bdays[0]["date"].get("year").is_none(), "--MMDD must have no year");
        // IMPP preserved.
        let impp = c.extra["impp"].as_array().unwrap();
        assert_eq!(impp[0].as_str(), Some("xmpp:carol@jabber.example"));
        // REV compact → normalized.
        assert_eq!(c.updated.as_deref(), Some("2024-02-25T02:04:08Z"));
    }

    #[test]
    fn photo_blob_stripped_url_kept() {
        let blob_card = parse_vcard_block(VCARD3_PHOTO_BLOB).expect("should parse");
        assert!(blob_card.photo.is_empty(), "base64 blob must be stripped");

        let url_card = parse_vcard_block(VCARD4_PHOTO_URL).expect("should parse");
        assert_eq!(url_card.photo, "https://example.com/jane.jpg");
    }

    #[test]
    fn block_with_no_identity_returns_none() {
        // No UID, no FN, no NAME, no email — no usable id.
        assert!(parse_vcard_block("VERSION:3.0\n").is_none());
    }

    #[test]
    fn apple_sentinel_year_1604_dropped_without_param() {
        let block = "UID:sentinel-test\nFN:Sentinel Sam\nBDAY:1604-03-15\n";
        let c = parse_vcard_block(block).expect("should parse");
        let bdays = c.extra["birthdays"].as_array().unwrap();
        let date = &bdays[0]["date"];
        assert!(date.get("year").is_none(), "sentinel 1604 year stripped: {date:?}");
        assert_eq!(date["month"], 3);
        assert_eq!(date["day"], 15);
    }

    #[test]
    fn emails_normalized_and_deduped() {
        let block = "UID:dedup-test\nFN:Dedup Test\n\
                     EMAIL:Alice@EXAMPLE.COM\nEMAIL:alice@example.com\nEMAIL:alice@other.com\n";
        let c = parse_vcard_block(block).expect("should parse");
        assert_eq!(c.emails, vec!["alice@example.com", "alice@other.com"]);
    }

    // ---------------------------------------------------------------------------
    // Vault snapshot tests (injected contacts, no network)

    #[test]
    fn snapshot_round_trip() {
        let v = temp_vault("roundtrip");
        let c1 = parse_vcard_block(VCARD3_ICLOUD).unwrap();
        let c2 = parse_vcard_block(VCARD4_FULL).unwrap();
        let stats = v.apply_icloud_contacts(vec![c1.clone(), c2], "").unwrap();
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.total, 2);
        assert!(!stats.skipped_no_credential);

        let rel = snapshot_rel("");
        let loaded: Vec<Contact> = v.read_snapshot(&rel).unwrap();
        assert_eq!(loaded.len(), 2);
        // Sorted by id.
        let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.windows(2).all(|w| w[0] <= w[1]), "ids are sorted: {ids:?}");
        // Full fidelity survived round-trip.
        let jane = loaded.iter().find(|c| c.source == "icloud-contacts").unwrap();
        assert_eq!(jane.emails, c1.emails);
    }

    #[test]
    fn snapshot_partitioned_by_apple_id() {
        let v = temp_vault("partitioned");
        let c = parse_vcard_block(VCARD3_ICLOUD).unwrap();
        let apple_id = "user@icloud.com";
        v.apply_icloud_contacts(vec![c.clone()], apple_id).unwrap();
        // Snapshot is at the partitioned path.
        let rel = snapshot_rel(apple_id);
        let loaded: Vec<Contact> = v.read_snapshot(&rel).unwrap();
        assert_eq!(loaded.len(), 1);
        // account field is stamped.
        assert_eq!(loaded[0].account, apple_id);
    }

    #[test]
    fn unchanged_snapshot_not_rewritten() {
        let v = temp_vault("nochange");
        let c = parse_vcard_block(VCARD3_ICLOUD).unwrap();
        v.apply_icloud_contacts(vec![c.clone()], "").unwrap();
        let path = v.root().join(snapshot_rel(""));
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        let stats = v.apply_icloud_contacts(vec![c], "").unwrap();
        assert_eq!(stats.changed, 0);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "mtime unchanged when no content change"
        );
    }

    #[test]
    fn change_count_adds_removes_edits() {
        let v = temp_vault("changes");
        let c1 = parse_vcard_block(VCARD3_ICLOUD).unwrap();
        v.apply_icloud_contacts(vec![c1.clone()], "").unwrap();

        let mut edited = c1.clone();
        edited.name = "Jane Edited".to_string();
        let c2 = parse_vcard_block(VCARD4_FULL).unwrap();
        let stats = v.apply_icloud_contacts(vec![edited, c2], "").unwrap();
        assert_eq!(stats.changed, 2, "one edit + one add");

        // Removing all two contacts is still a valid (non-empty → non-empty) path.
        let c3 = parse_vcard_block(VCARD3_NO_UID).unwrap();
        let stats = v.apply_icloud_contacts(vec![c3], "").unwrap();
        // Previous had 2 entries (edited + c2); now 1 different one = 3 changes.
        assert_eq!(stats.total, 1);
        assert!(stats.changed > 0);
    }

    #[test]
    fn empty_fetch_with_existing_snapshot_is_error() {
        // Returning 0 contacts when the snapshot is non-empty signals a parse
        // failure — the guard must refuse to wipe the snapshot.
        let v = temp_vault("empty-guard");
        let c = parse_vcard_block(VCARD3_ICLOUD).unwrap();
        v.apply_icloud_contacts(vec![c], "").unwrap();
        let err = v.apply_icloud_contacts(vec![], "").unwrap_err();
        assert!(
            err.to_string().contains("0 contacts"),
            "error message identifies the problem: {err}"
        );
    }

    #[test]
    fn empty_fetch_on_empty_snapshot_is_ok() {
        // First-run with no contacts: write empty snapshot (creates the file).
        let v = temp_vault("empty-ok");
        let stats = v.apply_icloud_contacts(vec![], "").unwrap();
        assert_eq!(stats.changed, 0);
        assert_eq!(stats.total, 0);
    }

    #[test]
    fn no_credential_silently_skips() {
        let v = temp_vault("nocred");
        let stats = v.collect_icloud_contacts().unwrap();
        assert!(stats.skipped_no_credential);
        assert_eq!(stats.changed, 0);
    }

    #[test]
    fn old_contact_line_still_deserializes() {
        // Back-compat: a minimal historical JSONL line (source+id only) parses.
        let minimal: Contact =
            serde_json::from_str(r#"{"source":"icloud-contacts","id":"OLD"}"#).unwrap();
        assert_eq!(minimal.id, "OLD");
        assert!(minimal.emails.is_empty() && minimal.extra.is_empty());
    }

    // ---------------------------------------------------------------------------
    // Credential parsing

    #[test]
    fn parse_credential_two_lines() {
        let cred =
            parse_pasted_credential("user@icloud.com\nxxxx-yyyy-zzzz-aaaa").unwrap();
        assert_eq!(cred.username, "user@icloud.com");
        assert_eq!(cred.password, "xxxx-yyyy-zzzz-aaaa");
    }

    #[test]
    fn parse_credential_blank_line_skipped() {
        let cred =
            parse_pasted_credential("  user@icloud.com  \n\n  mypassword  ").unwrap();
        assert_eq!(cred.username, "user@icloud.com");
        assert_eq!(cred.password, "mypassword");
    }

    #[test]
    fn parse_credential_one_line_returns_none() {
        assert!(parse_pasted_credential("user@icloud.com").is_none());
    }

    // ---------------------------------------------------------------------------
    // XML parsing helpers

    #[test]
    fn extract_dav_href_finds_principal() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:propstat><d:prop><d:current-user-principal><d:href>/principals/user/</d:href></d:current-user-principal></d:prop></d:propstat></d:response></d:multistatus>"#;
        let v = extract_dav_href(xml, "current-user-principal").unwrap();
        assert_eq!(v, "/principals/user/");
    }

    #[test]
    fn parse_multistatus_vcards_extracts_data() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/addressbooks/user/contacts/card1.vcf</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"abc123"</d:getetag>
        <card:address-data>BEGIN:VCARD
VERSION:3.0
UID:test-uid-001
FN:Test Contact
EMAIL:test@example.com
END:VCARD</card:address-data>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let items = parse_multistatus_vcards(xml);
        assert_eq!(items.len(), 1, "one response found");
        assert!(items[0].0.contains("card1.vcf"), "href: {}", items[0].0);
        assert!(items[0].2.contains("Test Contact"), "vcard data extracted");
    }

    #[test]
    fn parse_addressbook_list_finds_books() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/addressbooks/user/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>Home</d:displayname>
      </d:prop>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/addressbooks/user/contacts/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
        <d:displayname>My Contacts</d:displayname>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let books = parse_addressbook_list(xml, "https://contacts.icloud.com");
        // Only the addressbook collection, not the home folder.
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].display_name, "My Contacts");
        assert!(
            books[0].url.contains("/addressbooks/user/contacts/"),
            "url: {}",
            books[0].url
        );
    }

    #[test]
    fn parse_vcard_text_splits_and_parses_multi_card() {
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nUID:a\nFN:Alice\nEMAIL:a@example.com\nEND:VCARD\n\
                   BEGIN:VCARD\nVERSION:3.0\nUID:b\nFN:Bob\nEMAIL:b@example.com\nEND:VCARD\n";
        let contacts = parse_vcard_text(vcf);
        assert_eq!(contacts.len(), 2);
        assert!(contacts.iter().any(|c| c.id == "a"));
        assert!(contacts.iter().any(|c| c.id == "b"));
    }

    #[test]
    fn hub_card_shows_correct_domain() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "icloud-contacts").unwrap();
        assert_eq!(card.domain, "contacts");
        assert!(card.last_data.is_none() || card.last_data.as_deref() == Some(""));
    }

    // ---------------------------------------------------------------------------
    // XML entity unescaping

    #[test]
    fn xml_unescape_decodes_all_entities() {
        assert_eq!(xml_unescape("AT&amp;T"), "AT&T");
        assert_eq!(xml_unescape("a &lt; b &gt; c"), "a < b > c");
        assert_eq!(xml_unescape("say &quot;hi&quot;"), "say \"hi\"");
        assert_eq!(xml_unescape("it&apos;s fine"), "it's fine");
        // No entities → same string returned without allocation.
        assert_eq!(xml_unescape("plain text"), "plain text");
    }

    #[test]
    fn xml_unescape_applied_to_address_data() {
        // A CardDAV multistatus response where the org name contains an XML
        // entity (&amp;). The parser must unescape it before processing vCard.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/addressbooks/user/contacts/att.vcf</d:href>
    <d:propstat>
      <d:prop>
        <card:address-data>BEGIN:VCARD
VERSION:3.0
UID:att-uid-001
FN:AT&amp;T Rep
ORG:AT&amp;T;
NOTE:buy milk &lt;urgent&gt; &amp; eggs
END:VCARD</card:address-data>
      </d:prop>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let items = parse_multistatus_vcards(xml);
        assert_eq!(items.len(), 1, "one response extracted");
        let vcard_text = &items[0].2;
        // After unescaping: FN and ORG should contain the literal & character.
        assert!(
            vcard_text.contains("AT&T"),
            "FN/ORG should have & unescaped, got: {vcard_text:?}"
        );
        assert!(
            vcard_text.contains("buy milk <urgent> & eggs"),
            "NOTE should have entities unescaped, got: {vcard_text:?}"
        );
        // Parse the contact and confirm the org name is clean.
        let contacts = parse_vcard_text(vcard_text);
        assert_eq!(contacts.len(), 1);
        let c = &contacts[0];
        assert_eq!(c.name, "AT&T Rep", "FN decoded: {}", c.name);
        let org_name = c.orgs.first().and_then(|o| o.name.as_deref()).unwrap_or("");
        assert_eq!(org_name, "AT&T", "ORG decoded: {org_name}");
        let note = c.extra.get("note").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(note, "buy milk <urgent> & eggs", "NOTE decoded: {note}");
    }
}
