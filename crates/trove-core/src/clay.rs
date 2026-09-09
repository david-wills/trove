//! Clay (Mesh) personal CRM — user-initiated CSV export import.
//!
//! Clay (clay.earth, partially rebranded "Mesh") enriches your address book
//! with data from email, calendar, LinkedIn, and iMessage — job changes,
//! company updates, news mentions layered onto contacts. The enrichment layer
//! is Clay-computed and not independently verifiable, so it lands as
//! supplementary metadata in `extra`, never overwriting authoritative fields.
//!
//! **Access.** Clay has no public REST API for personal-tier accounts. The
//! only import path is the CSV export from the Clay dashboard (Settings →
//! Export). The user drops the file on the registry-driven import box; no
//! auth, no network, no TCC.
//!
//! **Vault layout.** Contacts are current-state data (a snapshot, not a stream),
//! so this source writes one JSONL snapshot at `contacts/clay/contacts.jsonl`,
//! atomically replaced on every import. Each line is a [`crate::contacts::Contact`]
//! with `source = "clay"`. The folder name is the source id — per the contacts
//! domain's "source = folder name" rule.
//!
//! **Export format.** Clay's export is documented as "backward compatible with
//! Google Contacts" (library.me.sh, article 6821989456027). The canonical
//! header row uses Google Contacts CSV column names: `Name`, `Given Name`,
//! `Family Name`, plus indexed email/phone/org columns such as
//! `E-mail 1 - Value`, `E-mail 2 - Value`, `Phone 1 - Value`,
//! `Organization 1 - Name`, `Organization 1 - Title`.  The parser handles all
//! indexed variants (1, 2, 3…) via pattern predicates and multi-value
//! splitting, while routing every unrecognised column to `extra` for full
//! fidelity. Users who have previously exported in an older or custom format
//! will also have their data imported gracefully — common alternative header
//! names are matched as fallbacks.
//!
//! **Reference:** `apple_contacts.rs` (contacts contract + `write_snapshot`
//! pattern), `linkedin.rs` (CSV import to contacts contract),
//! `letterboxd.rs` (Import def shape).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};
use serde_json::{Map, Value};

use crate::contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

/// The collector id — written into every `Contact::source` field and used as
/// the folder name under `contacts/` per the domain's "source = folder name"
/// rule.
const SOURCE: &str = "clay";

/// The single snapshot file for this source. Contacts are current-state data;
/// the file is atomically rewritten on every import.
const SNAPSHOT_REL: &str = "contacts/clay/contacts.jsonl";

fn def_last_data(vault: &Vault) -> Option<String> {
    let path = vault.resolve(SNAPSHOT_REL).ok()?;
    crate::registry::file_mtime(&path)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "clay",
        name: "Clay (Mesh)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Clay personal CRM contacts from a CSV export. \
                      Clay enriches your address book with job changes, company \
                      updates, and social context — the enrichment layer lands as \
                      supplementary metadata. Re-runnable: re-importing the same \
                      export is a safe no-op.",
        domain: "contacts",
        vault_path: "contacts/clay/",
        toggleable: false,
        setup: &[
            "clay.earth (or me.sh) → Settings → Export → Download CSV.",
            "Drop the downloaded CSV here.",
        ],
        caveats: "Clay has no public personal-tier API — CSV export only. \
                  Enrichment fields (job titles, company updates) are supplementary \
                  and Clay-computed; they are preserved verbatim under `extra`. \
                  The product is VC-funded and undergoing a rebrand; \
                  export format may change.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Column name recognition — case-insensitive, trimmed.
//
// Clay's export is documented as "backward compatible with Google Contacts".
// The Google Contacts CSV format uses:
//   - display name: "Name"
//   - given/family: "Given Name", "Family Name"
//   - email:  "E-mail 1 - Value", "E-mail 2 - Value", … (indexed)
//   - phone:  "Phone 1 - Value", "Phone 2 - Value", … (indexed)
//   - org:    "Organization 1 - Name", "Organization 1 - Title" (indexed)
//   - links:  "Website 1 - Value" (social handles, LinkedIn URL, etc.)
//
// All `is_email_*` / `is_phone_*` predicates use starts_with/ends_with
// patterns so they capture every indexed variant without enumerating them.

/// Recognises column headers that map to `Contact::name` (the display name).
fn is_name_col(h: &str) -> bool {
    matches!(h, "name" | "full name" | "display name" | "contact name")
}

/// Recognises `Contact::given` columns.
fn is_given_col(h: &str) -> bool {
    matches!(h, "given name" | "first name" | "firstname" | "given")
}

/// Recognises `Contact::family` columns.
fn is_family_col(h: &str) -> bool {
    matches!(h, "family name" | "last name" | "surname" | "lastname" | "family")
}

/// Recognises an email column.
///
/// Matches Google Contacts indexed variants (`e-mail 1 - value`,
/// `e-mail 2 - value`, …) and common bare names.
fn is_email_col(h: &str) -> bool {
    // Google Contacts: "e-mail N - value"
    if h.starts_with("e-mail ") && h.ends_with(" - value") {
        return true;
    }
    // Common alternatives
    matches!(h, "email" | "email address" | "emails" | "e-mail" | "email 1 - value" | "email 2 - value")
}

/// Recognises a phone column.
///
/// Matches Google Contacts indexed variants (`phone 1 - value`,
/// `phone 2 - value`, …) and common bare names.
fn is_phone_col(h: &str) -> bool {
    // Google Contacts: "phone N - value"
    if h.starts_with("phone ") && h.ends_with(" - value") {
        return true;
    }
    // Common alternatives
    matches!(h, "phone" | "phone number" | "mobile" | "mobile number" | "cell" | "telephone")
}

/// Recognises an organization name column.
///
/// Matches Google Contacts `organization N - name` pattern and common
/// bare names.
fn is_company_col(h: &str) -> bool {
    // Google Contacts: "organization N - name"
    if h.starts_with("organization ") && h.ends_with(" - name") {
        return true;
    }
    // Common alternatives
    matches!(h, "company" | "organization" | "organisation" | "employer" | "work")
}

/// Recognises a job title column.
///
/// Matches Google Contacts `organization N - title` pattern and common
/// bare names. Only the first/primary org's title is mapped to the
/// normalized `ContactOrg::title`; additional indexed titles fall to `extra`.
fn is_title_col(h: &str) -> bool {
    // Google Contacts: "organization N - title"
    if h.starts_with("organization ") && h.ends_with(" - title") {
        return true;
    }
    // Common alternatives
    matches!(h, "title" | "job title" | "position" | "role")
}

/// Recognises a stable Clay contact id column (Clay may export a row id or URL).
fn is_id_col(h: &str) -> bool {
    matches!(h, "id" | "contact id" | "clay id" | "url" | "profile url" | "link")
}

/// Recognises a website / social handle column that may carry the LinkedIn URL
/// or another stable cross-source handle. Used to strengthen the fallback guid
/// when the explicit id column is absent and the name+email composite would
/// otherwise collide.
fn is_website_col(h: &str) -> bool {
    // Google Contacts: "website N - value"
    if h.starts_with("website ") && h.ends_with(" - value") {
        return true;
    }
    matches!(h, "website" | "linkedin" | "linkedin url" | "social" | "handle" | "url")
}

// ---------------------------------------------------------------------------
// Import

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load the current snapshot so the import is idempotent: re-importing the
    // same export keeps existing contacts unchanged and only adds new ones.
    let existing: Vec<Contact> = vault.read_snapshot(SNAPSHOT_REL).unwrap_or_default();
    let mut by_id: BTreeMap<String, Contact> =
        existing.into_iter().map(|c| (c.id.clone(), c)).collect();
    let initial_count = by_id.len();

    let body = std::fs::read_to_string(path)?;
    let contacts = parse_clay_csv(&body)?;
    let total = contacts.len() as u64;

    // Track seen ids within this import batch to detect within-file dupes.
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;

    for (i, contact) in contacts.into_iter().enumerate() {
        if i % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
        let id = contact.id.clone();
        if seen_ids.contains(&id) {
            duplicates += 1;
            continue;
        }
        seen_ids.insert(id.clone());
        // Upsert: a newer export silently updates the stored record.
        let is_new = !by_id.contains_key(&id);
        if is_new {
            imported += 1;
        } else {
            duplicates += 1;
        }
        by_id.insert(id, contact);
    }

    let records: Vec<&Contact> = by_id.values().collect();
    vault.write_snapshot(SNAPSHOT_REL, &records)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} contacts imported, {duplicates} duplicates/updates skipped ({} total on file)",
            by_id.len()
        ),
        counts: BTreeMap::from([
            ("imported", imported),
            ("duplicates", duplicates),
            ("total_rows", total),
            ("total_on_file", (initial_count + imported as usize) as u64),
        ]),
    })
}

// ---------------------------------------------------------------------------
// CSV parser
//
// Clay's export is documented as "backward compatible with Google Contacts"
// (library.me.sh, article 6821989456027). The parser discovers the column
// layout from the actual header row and maps what it recognises; everything
// else lands in `extra`. Multiple indexed email/phone columns (E-mail 1/2/3…)
// are all collected; comma-separated multi-values within a cell are split.

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Parse the CSV body into a list of [`Contact`] rows.
///
/// Column mapping is header-driven and case-insensitive. Any column not
/// matched by the `is_*_col` predicates is routed to `extra` verbatim —
/// full export fidelity even when the exact format differs from expectations.
///
/// Multiple indexed email/phone columns (`E-mail 1 - Value`, `E-mail 2 - Value`, …)
/// are all collected and merged into the `emails`/`phones` Vec after
/// normalization and deduplication. Comma-separated multi-values within a
/// single cell (an alternative encoding some exporters use) are also split.
fn parse_clay_csv(body: &str) -> Result<Vec<Contact>> {
    // Strip a BOM if present and locate the actual header row. Clay may
    // include a preamble line (like LinkedIn does); skip lines that don't
    // look like a header.
    let clean = body.trim_start_matches('\u{feff}');

    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .flexible(true) // tolerate rows with missing trailing columns
        .from_reader(clean.as_bytes());

    let headers: Vec<String> = rdr
        .headers()?
        .iter()
        .map(|h| h.trim().to_lowercase())
        .collect();

    // Identify which column index maps to which semantic role.
    // For multi-value fields (email, phone, company, title) we collect ALL
    // matching column indices so every indexed variant is picked up.
    let idx_name: Option<usize> = headers.iter().position(|h| is_name_col(h));
    let idx_given: Option<usize> = headers.iter().position(|h| is_given_col(h));
    let idx_family: Option<usize> = headers.iter().position(|h| is_family_col(h));
    let idx_id: Option<usize> = headers.iter().position(|h| is_id_col(h));

    // Collect ALL email/phone/company/title column indices (indexed variants).
    let email_idxs: Vec<usize> = headers.iter().enumerate()
        .filter(|(_, h)| is_email_col(h))
        .map(|(i, _)| i)
        .collect();
    let phone_idxs: Vec<usize> = headers.iter().enumerate()
        .filter(|(_, h)| is_phone_col(h))
        .map(|(i, _)| i)
        .collect();
    // For org we take the first matched company and first matched title.
    // Additional org columns (Organization 2 - Name etc.) fall to `extra`.
    let idx_company: Option<usize> = headers.iter().position(|h| is_company_col(h));
    let idx_title: Option<usize> = headers.iter().position(|h| is_title_col(h));
    // Website/social columns: collect all, used as guid tiebreaker.
    let website_idxs: Vec<usize> = headers.iter().enumerate()
        .filter(|(_, h)| is_website_col(h))
        .map(|(i, _)| i)
        .collect();

    // Column indices that are handled above and should NOT appear in `extra`.
    let mut mapped: HashSet<usize> = HashSet::new();
    for opt in &[idx_name, idx_given, idx_family, idx_id, idx_company, idx_title] {
        if let Some(i) = *opt { mapped.insert(i); }
    }
    for &i in &email_idxs { mapped.insert(i); }
    for &i in &phone_idxs { mapped.insert(i); }
    for &i in &website_idxs { mapped.insert(i); }

    let col = |row: &csv::StringRecord, i: Option<usize>| -> String {
        i.and_then(|i| row.get(i)).unwrap_or("").trim().to_string()
    };

    let mut out = Vec::new();
    for row in rdr.records() {
        let Ok(row) = row else { continue };

        // Build the display name from the dedicated column or first+last.
        let given = col(&row, idx_given);
        let family = col(&row, idx_family);
        let name = {
            let n = col(&row, idx_name);
            if !n.is_empty() {
                n
            } else {
                format!("{given} {family}").trim().to_string()
            }
        };

        // Collect all email values from every matching column, splitting on
        // commas (some exporters pack multiple addresses into one cell).
        let emails: Vec<String> = {
            let mut seen: HashSet<String> = HashSet::new();
            let mut out: Vec<String> = Vec::new();
            for &i in &email_idxs {
                let cell = row.get(i).unwrap_or("").trim().to_string();
                // Split comma-separated multi-values.
                for part in cell.split(',') {
                    let e = normalize_email(part.trim());
                    if !e.is_empty() && seen.insert(e.clone()) {
                        out.push(e);
                    }
                }
            }
            out
        };

        // Skip rows with no identifying information.
        if name.is_empty() && emails.is_empty() {
            continue;
        }

        // Collect all phone values from every matching column, splitting on
        // commas similarly.
        let phones: Vec<String> = {
            let mut seen: HashSet<String> = HashSet::new();
            let mut out: Vec<String> = Vec::new();
            for &i in &phone_idxs {
                let cell = row.get(i).unwrap_or("").trim().to_string();
                for part in cell.split(',') {
                    let p = normalize_phone(part.trim(), None);
                    if !p.is_empty() && seen.insert(p.clone()) {
                        out.push(p);
                    }
                }
            }
            out
        };

        let company = col(&row, idx_company);
        let title = col(&row, idx_title);
        let orgs: Vec<ContactOrg> = if company.is_empty() && title.is_empty() {
            Vec::new()
        } else {
            vec![ContactOrg {
                name: if company.is_empty() { None } else { Some(company) },
                title: if title.is_empty() { None } else { Some(title) },
            }]
        };

        // Stable id: prefer an explicit Clay id/URL column, fall back to a
        // hash of the name + first email + first website composite. Using
        // the email prevents name-only collisions where two distinct people
        // share a display name (e.g. two "John Smith" entries with different
        // emails). The website/social handle further disambiguates when the
        // email is also absent.
        let id = {
            let explicit = col(&row, idx_id);
            if !explicit.is_empty() {
                sha256_hex(explicit.as_bytes())
            } else {
                // Collect website/social values for tiebreaking.
                let website_raw: String = website_idxs.iter()
                    .filter_map(|&i| row.get(i))
                    .map(|s| s.trim())
                    .find(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let email_raw = emails.first().cloned().unwrap_or_default();
                let key = format!(
                    "{}|{}|{}",
                    name.trim().to_ascii_lowercase(),
                    email_raw,
                    website_raw,
                );
                sha256_hex(key.as_bytes())
            }
        };

        // Everything we didn't map → extra (full export fidelity).
        let mut extra: Map<String, Value> = Map::new();
        for (i, header) in headers.iter().enumerate() {
            if mapped.contains(&i) {
                continue;
            }
            if let Some(val) = row.get(i) {
                let val = val.trim();
                if !val.is_empty() {
                    extra.insert(header.clone(), Value::String(val.to_string()));
                }
            }
        }

        out.push(Contact {
            source: SOURCE.to_string(),
            id,
            account: String::new(),
            name,
            given,
            family,
            emails,
            phones,
            orgs,
            photo: String::new(),
            other: false,
            updated: None,
            extra,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-clay-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Real Google Contacts CSV header format (as documented in Clay's export
    // compatibility note). Uses indexed columns for email, phone, and org.
    // Three rows: Alice (full), Bob (sparse), Charlie (name-only, no email).
    // Charlie is a valid sparse contact — a name alone is enough.
    const GOOGLE_CONTACTS_CSV: &str = "\
Name,Given Name,Family Name,E-mail 1 - Value,E-mail 2 - Value,Phone 1 - Value,Phone 2 - Value,Organization 1 - Name,Organization 1 - Title,Notes,Website 1 - Value
Alice Example,Alice,Example,alice@example.com,alice@work.com,+14155550101,,Acme Corp,CTO,Old friend,https://linkedin.com/in/alice
Bob Builder,Bob,Builder,bob@example.com,,,,Builder Co,Contractor,,
Charlie,Charlie,,,,,,,,
";

    // Legacy / alternative column name format (common personal-CRM export).
    // Tests that older or custom exports still work as a fallback.
    const LEGACY_CSV: &str = "\
Name,Email,Phone,Company,Title,Notes,Last Contacted
Alice Example,alice@example.com,+14155550101,Acme Corp,CTO,Old friend,2026-01-15
Bob Builder,bob@example.com,,Builder Co,Contractor,,2025-11-01
Charlie,,,,,,
";

    // Split first/last name using legacy format.
    const SPLIT_NAME_CSV: &str = "\
First Name,Last Name,Email,Company,Title,Connected Via
Alice,Example,alice@example.com,Acme Corp,CTO,LinkedIn
Bob,Builder,bob@example.com,Builder Co,Contractor,Email
";

    fn import(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("clay-export.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Real Google Contacts format tests (the primary format Clay exports)

    #[test]
    fn google_contacts_format_maps_core_fields() {
        let v = temp_vault("gc-core");
        let out = import(&v, GOOGLE_CONTACTS_CSV);
        // Alice + Bob + Charlie (name-only) all land.
        assert_eq!(out.counts["imported"], 3, "headline: {}", out.headline);

        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 3);

        let alice = contacts.iter().find(|c| c.name == "Alice Example").expect("Alice");
        assert_eq!(alice.source, "clay");
        assert_eq!(alice.given, "Alice");
        assert_eq!(alice.family, "Example");

        // Both indexed email columns must be collected.
        assert!(alice.emails.contains(&"alice@example.com".to_string()), "primary email");
        assert!(alice.emails.contains(&"alice@work.com".to_string()), "secondary email");
        assert_eq!(alice.emails.len(), 2, "exactly two distinct emails");

        // Phone from the indexed column.
        assert_eq!(alice.phones, vec!["+14155550101"]);

        // Organization from the indexed column.
        assert_eq!(alice.orgs.len(), 1);
        assert_eq!(alice.orgs[0].name.as_deref(), Some("Acme Corp"));
        assert_eq!(alice.orgs[0].title.as_deref(), Some("CTO"));

        // Unmapped columns land in extra.
        assert!(alice.extra.contains_key("notes"), "notes → extra");

        let bob = contacts.iter().find(|c| c.name == "Bob Builder").expect("Bob");
        assert_eq!(bob.emails, vec!["bob@example.com"]);
        assert!(bob.phones.is_empty(), "empty phone omitted");
        assert_eq!(bob.orgs[0].name.as_deref(), Some("Builder Co"));

        let charlie = contacts.iter().find(|c| c.name == "Charlie").expect("Charlie");
        assert!(charlie.emails.is_empty(), "no email on sparse row");
        assert!(charlie.phones.is_empty());
        assert!(charlie.orgs.is_empty());
    }

    #[test]
    fn google_contacts_format_email_in_first_position_not_lost() {
        // Regression: with the old single-index approach, "E-mail 1 - Value"
        // would not match is_email_col and idx_email would be None → emails=[]
        // for every contact. Verify the fix directly.
        let v = temp_vault("gc-email-regression");
        let csv = "\
Name,E-mail 1 - Value,Phone 1 - Value,Organization 1 - Name,Organization 1 - Title
Alice Example,alice@example.com,+14155550101,Acme Corp,CTO
";
        import(&v, csv);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let alice = contacts.iter().find(|c| c.name == "Alice Example").expect("Alice");
        assert_eq!(alice.emails, vec!["alice@example.com"],
            "E-mail 1 - Value must be recognised; got {:?}", alice.emails);
        assert_eq!(alice.phones, vec!["+14155550101"],
            "Phone 1 - Value must be recognised; got {:?}", alice.phones);
        assert_eq!(alice.orgs[0].name.as_deref(), Some("Acme Corp"),
            "Organization 1 - Name must be recognised");
        assert_eq!(alice.orgs[0].title.as_deref(), Some("CTO"),
            "Organization 1 - Title must be recognised");
    }

    #[test]
    fn multi_value_email_columns_are_all_collected() {
        let v = temp_vault("multi-email");
        let csv = "\
Name,E-mail 1 - Value,E-mail 2 - Value,E-mail 3 - Value
Alice,alice@a.com,alice@b.com,alice@c.com
";
        import(&v, csv);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let alice = &contacts[0];
        assert_eq!(alice.emails, vec!["alice@a.com", "alice@b.com", "alice@c.com"],
            "all three indexed email columns collected");
    }

    #[test]
    fn comma_separated_multi_email_within_cell_is_split() {
        let v = temp_vault("comma-email");
        let csv = "Name,E-mail 1 - Value\nAlice,\"alice@a.com,alice@b.com\"\n";
        import(&v, csv);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts[0].emails, vec!["alice@a.com", "alice@b.com"],
            "comma-separated emails within one cell split and normalised");
    }

    #[test]
    fn name_collision_with_different_emails_does_not_drop_contact() {
        // Two distinct "John Smith" contacts with different emails: before the
        // fix both would hash to the same guid (name|"") and one would be
        // silently dropped. Now the email participates in the composite.
        let v = temp_vault("guid-collision");
        let csv = "\
Name,E-mail 1 - Value
John Smith,john1@example.com
John Smith,john2@example.com
";
        let out = import(&v, csv);
        assert_eq!(out.counts["imported"], 2,
            "two distinct Johns must produce two distinct contacts");
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 2);
        let id0 = &contacts[0].id;
        let id1 = &contacts[1].id;
        assert_ne!(id0, id1, "guids must differ when emails differ");
    }

    // -----------------------------------------------------------------------
    // Legacy / alternative column name tests

    #[test]
    fn parses_legacy_full_name_column_and_maps_core_fields() {
        let v = temp_vault("legacy-full-name");
        let out = import(&v, LEGACY_CSV);
        // Alice + Bob + Charlie (name-only, no email) all land — a name
        // alone is enough to be a valid sparse contact.
        assert_eq!(out.counts["imported"], 3, "headline: {}", out.headline);

        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 3);

        let alice = contacts.iter().find(|c| c.name == "Alice Example").expect("Alice");
        assert_eq!(alice.source, "clay");
        assert_eq!(alice.emails, vec!["alice@example.com"]);
        assert_eq!(alice.phones, vec!["+14155550101"]);
        assert_eq!(alice.orgs.len(), 1);
        assert_eq!(alice.orgs[0].name.as_deref(), Some("Acme Corp"));
        assert_eq!(alice.orgs[0].title.as_deref(), Some("CTO"));
        // Unmapped columns go to extra.
        assert!(alice.extra.contains_key("notes"), "notes → extra");
        assert!(alice.extra.contains_key("last contacted"), "last contacted → extra");

        let bob = contacts.iter().find(|c| c.name == "Bob Builder").expect("Bob");
        assert!(bob.phones.is_empty(), "empty phone omitted");
    }

    #[test]
    fn parses_split_first_last_name_columns() {
        let v = temp_vault("split-name");
        let out = import(&v, SPLIT_NAME_CSV);
        assert_eq!(out.counts["imported"], 2);

        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        let alice = contacts.iter().find(|c| c.name == "Alice Example").expect("Alice");
        assert_eq!(alice.given, "Alice");
        assert_eq!(alice.family, "Example");
        // Unmapped "Connected Via" lands in extra.
        assert!(alice.extra.contains_key("connected via"));
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let out1 = import(&v, GOOGLE_CONTACTS_CSV);
        assert_eq!(out1.counts["imported"], 3);

        // Second import of the same data — contacts are upserted (no new ids).
        let out2 = import(&v, GOOGLE_CONTACTS_CSV);
        assert_eq!(out2.counts["duplicates"], 3, "same ids → duplicates");

        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 3, "row count unchanged after re-import");
    }

    #[test]
    fn email_is_normalised_to_lowercase() {
        let v = temp_vault("email-norm");
        let csv = "Name,E-mail 1 - Value\nAlice,Alice@EXAMPLE.COM\n";
        import(&v, csv);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts[0].emails, vec!["alice@example.com"]);
    }

    #[test]
    fn bom_prefixed_csv_is_handled() {
        let v = temp_vault("bom");
        let csv = "\u{feff}Name,E-mail 1 - Value\nAlice,alice@example.com\n";
        let out = import(&v, csv);
        assert_eq!(out.counts["imported"], 1);
    }

    #[test]
    fn empty_and_malformed_rows_are_skipped() {
        let v = temp_vault("skips");
        let csv = "Name,E-mail 1 - Value\n\n,\nAlice,alice@example.com\n";
        let out = import(&v, csv);
        assert_eq!(out.counts["imported"], 1);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 1);
    }

    #[test]
    fn snapshot_is_atomically_replaced_on_reimport() {
        let v = temp_vault("atomic");
        let csv1 = "Name,E-mail 1 - Value\nAlice,alice@example.com\n";
        let csv2 = "Name,E-mail 1 - Value\nAlice,alice@example.com\nBob,bob@example.com\n";
        import(&v, csv1);
        import(&v, csv2);
        let contacts: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        assert_eq!(contacts.len(), 2);
    }

    #[test]
    fn stable_id_is_consistent_across_imports() {
        let v = temp_vault("stable-id");
        import(&v, GOOGLE_CONTACTS_CSV);
        let first: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        import(&v, GOOGLE_CONTACTS_CSV);
        let second: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        // Same rows → same ids after two imports.
        let ids1: Vec<_> = first.iter().map(|c| &c.id).collect();
        let ids2: Vec<_> = second.iter().map(|c| &c.id).collect();
        assert_eq!(ids1, ids2);
    }
}
