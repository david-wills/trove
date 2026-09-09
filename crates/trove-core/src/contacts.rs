//! The `contacts` domain contract: every address book and personal CRM in
//! one normalized, source-agnostic store.
//!
//! Each source keeps its own folder of full-fidelity rows under
//! `contacts/<source>/<account>.jsonl` — `<source>` is the collector id and
//! the folder name (the contract's "source = folder name" rule), `<account>`
//! is whatever partitions the source (a Google OAuth `sub`, an Apple ID, or
//! just `contacts` for a single-store import). Each file is a **snapshot**:
//! rewritten whole and atomically on every sync (sibling tmp + rename via
//! [`crate::store::Vault::write_snapshot`]); there is no event stream and no
//! `ts`. The dedupe key is [`Contact::id`], the source's stable per-contact
//! id.
//!
//! Collectors only *report* — they never merge. The read-time
//! entity-resolution layer joins people across sources on their clean
//! handles, which is why the handle convention is enforced hard at write
//! time: emails lowercased ([`normalize_email`]), phones E.164 where the
//! region is derivable ([`normalize_phone`]). A wrong merge baked into a row
//! is permanent; a clean handle is joinable forever. Both helpers are public
//! so every collector (Google, Apple, a `.vcf` import, a CRM) shares one
//! normalization discipline. Anything a source carries that the normalized
//! columns don't map — birthdays, addresses, urls, notes, CRM enrichment —
//! rides verbatim in [`Contact::extra`], full fidelity at write time.
//!
//! See [`docs/vault-spec/domains/contacts.md`] for the field-level spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

fn is_false(b: &bool) -> bool {
    !*b
}

/// One normalized contact — one line of `contacts/<source>/<account>.jsonl`.
///
/// A *current-state* record, not an event: there is no `ts`. Only `source`
/// and `id` are required; a sparse source (an other-contact with just an
/// email, a LinkedIn row with no email) writes a minimal line, a rich
/// address book fills more. The core columns are what entity resolution and
/// any reader actually key on; everything source-specific the columns don't
/// carry is preserved verbatim under [`extra`](Contact::extra) rather than
/// dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Contact {
    /// Collector id, identical to the source folder name (`google-contacts`,
    /// `apple-contacts`, `linkedin`, …). Always serialized.
    pub source: String,
    /// Source-native stable contact id — the dedupe key (Google
    /// `resourceName`, `CNContact.identifier`, vCard `UID`, a CRM row id, or
    /// a name+email composite where the export carries none). Always
    /// serialized.
    pub id: String,
    /// The connected account this contact belongs to (Google account
    /// address; absent for single-store imports).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// Display name, from the primary name entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Given name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub given: String,
    /// Family name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub family: String,
    /// Email handles, lowercased, in source order, exact duplicates removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
    /// Phone handles, E.164 where the region is derivable, else verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<String>,
    /// Organizations / job titles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orgs: Vec<ContactOrg>,
    /// Profile photo URL (real photos only; base64 blobs are stripped, never
    /// inlined).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub photo: String,
    /// Auto-collected rather than user-saved (Google's "Other contacts" —
    /// everyone you've corresponded with).
    #[serde(default, skip_serializing_if = "is_false")]
    pub other: bool,
    /// RFC3339 local time the contact last changed at the source, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    /// Everything source-specific the normalized fields don't carry —
    /// birthdays, addresses, urls, notes, relationship context
    /// (`connected_on`, `last_talked_to`), CRM tags/enrichment — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One organization affiliation on a contact: company and/or job title
/// (LinkedIn Company/Position, vCard ORG/TITLE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ContactOrg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Normalize an email handle to the contract's join form: trimmed and
/// lowercased. The exact-match entity-resolution layer keys on this, so the
/// convention is enforced at write time, the same way for every source.
pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Normalize a phone handle toward E.164. Returns the E.164 form whenever the
/// number parses — an already-`+`-prefixed international number, or a national
/// number plus a `default_region` hint (an ISO-3166 2-letter code like
/// `"US"`); otherwise returns the trimmed input verbatim, never guessing a
/// region. A handle that can't be made canonical is still useful raw, and a
/// wrong country prefix would be a permanent bad join key — so falling back
/// to verbatim is deliberate.
pub fn normalize_phone(raw: &str, default_region: Option<&str>) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // `phonenumber::parse` takes an optional region Id; a `+` international
    // number ignores it, a national number needs it. An unparseable region
    // hint is treated as "no hint" (parse may still succeed on a `+` number).
    let region = default_region.and_then(|r| r.trim().parse::<phonenumber::country::Id>().ok());
    match phonenumber::parse(region, trimmed) {
        Ok(number) if phonenumber::is_valid(&number) => {
            number.format().mode(phonenumber::Mode::E164).to_string()
        }
        // Parsed but not a valid number for its region, or didn't parse at
        // all: keep what the source gave us rather than emit a wrong handle.
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn email_is_trimmed_and_lowercased() {
        assert_eq!(normalize_email("  Alice@Example.COM "), "alice@example.com");
        assert_eq!(normalize_email("already@lower.com"), "already@lower.com");
    }

    #[test]
    fn phone_international_formats_to_e164_without_a_region() {
        assert_eq!(normalize_phone("+1 (415) 555-0142", None), "+14155550142");
        assert_eq!(normalize_phone("  +44 20 7946 0958 ", None), "+442079460958");
    }

    #[test]
    fn phone_national_uses_the_region_hint() {
        assert_eq!(normalize_phone("(415) 555-0142", Some("US")), "+14155550142");
        // Same digits, different region → different E.164.
        assert_eq!(normalize_phone("020 7946 0958", Some("GB")), "+442079460958");
    }

    #[test]
    fn phone_unparseable_falls_back_to_trimmed_verbatim() {
        // No region hint and not international → can't be made canonical.
        assert_eq!(normalize_phone("  555-0142  ", None), "555-0142");
        // Free-form extension text isn't a number — keep it verbatim.
        assert_eq!(normalize_phone("ext. 1234", None), "ext. 1234");
        assert_eq!(normalize_phone("", None), "");
        // A bogus region hint degrades to "no hint", not a panic.
        assert_eq!(normalize_phone("555-0142", Some("ZZ")), "555-0142");
    }

    #[test]
    fn minimal_record_serializes_only_source_and_id() {
        let c = Contact {
            source: "linkedin".into(),
            id: "row-1".into(),
            account: String::new(),
            name: String::new(),
            given: String::new(),
            family: String::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            orgs: Vec::new(),
            photo: String::new(),
            other: false,
            updated: None,
            extra: Map::new(),
        };
        // Omit-empty: a sparse line is exactly `{"source","id"}`.
        assert_eq!(serde_json::to_value(&c).unwrap(), json!({"source": "linkedin", "id": "row-1"}));
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "source": "google-contacts",
            "id": "people/c1",
            "account": "me@gmail.com",
            "name": "Alice Example",
            "given": "Alice",
            "family": "Example",
            "emails": ["alice@example.com", "alice@work.com"],
            "phones": ["+14155550142"],
            "orgs": [{"name": "Example Corp", "title": "CTO"}],
            "photo": "https://lh3.googleusercontent.com/contacts/real.jpg",
            "other": true,
            "updated": "2026-06-02T00:00:00Z",
            "extra": {"birthdays": [{"date": {"month": 3, "day": 14}}]}
        });
        let c: Contact = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(c.id, "people/c1");
        assert_eq!(c.emails, vec!["alice@example.com", "alice@work.com"]);
        assert!(c.other);
        assert!(c.extra.contains_key("birthdays"));
        assert_eq!(serde_json::to_value(&c).unwrap(), line);
    }
}
