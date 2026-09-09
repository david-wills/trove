//! Apple Contacts (CNContactStore) — the macOS unified address book, the
//! anchor of Trove's whole person layer. One TCC prompt
//! (`kTCCServiceAddressBook`) yields every contact across every account the
//! Mac syncs — iCloud, Google, Exchange, CardDAV, LDAP all federate into one
//! `CNContactStore`, deduped natively — including birthdays, anniversaries,
//! social handles, postal addresses, and notes.
//!
//! **Target.** Contacts are *current-state* data, not an event stream, so the
//! store is the [`crate::contacts`] domain's `Snapshot` kind: one atomically
//! rewritten JSONL file ([`Vault::write_snapshot`]) at
//! `contacts/apple-contacts/contacts.jsonl` — the folder is the source id
//! (`apple-contacts`), and there is a single unified file (`contacts`) because
//! CNContactStore is one federated store with no per-account split. One
//! [`crate::contacts::Contact`] per line, sorted by `id` (the CNContact
//! `identifier`) so successive rewrites diff cleanly. Each record is the
//! normalized cross-source core (names, emails, phones, organizations) with
//! everything Apple carries that the columns don't — birthdays, labeled dates,
//! postal addresses, urls, social profiles, notes — parked verbatim under
//! `extra`. Handles follow the domain convention: emails lowercased, phones
//! E.164 where derivable ([`crate::contacts::normalize_email`],
//! [`crate::contacts::normalize_phone`]).
//!
//! **TCC, mirroring [`crate::eventkit`].** `authorizationStatusForEntityType`
//! → an [`AuthStatus`]; the DEF permission hook reports it. The pull: when
//! Granted, enumerate; when NotDetermined, fire `requestAccessForEntityType`
//! (the system dialog the *user* approves — never auto-approved) and proceed
//! only if it returns granted; when Denied, a graceful no-error outcome
//! ("Contacts permission needed"). The prompt only renders when the process
//! carries `NSContactsUsageDescription` — an Info.plist concern owned by the
//! app and troved's embedded plist, *not* this crate (note only).
//!
//! **Testable split (the FFI can't run in tests).** The `#[cfg(macos)]` FFI
//! extracts each `CNContact` into a plain-Rust [`RawAppleContact`] (raw
//! strings, no normalization); the pure, cross-platform [`map_contact`] does
//! *all* normalization and is the unit-tested core. The thin FFI→intermediate
//! layer is validated live (a Needs-David, gated on the TCC grant). The
//! raw-SQLite `AddressBook-v22.abcddb` path is deliberately rejected (heavier
//! FDA grant, loses native cross-account dedup, schema-fragile) — framework
//! only.

use anyhow::Result;
use chrono::Local;
use serde_json::{Map, Value};

use crate::contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
use crate::integrations::{Integration, IntegrationKind, PermissionInfo};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::vault::Vault;

/// This collector's id — also the source folder name under `contacts/`
/// (`contacts/apple-contacts/contacts.jsonl`), per the contract's "source =
/// folder name" rule. Written into every record's `source` field.
const SOURCE: &str = "apple-contacts";

/// The single snapshot file. CNContactStore is one federated, natively-deduped
/// store, so there is no per-account partition — the contract's `<account>`
/// slot is simply `contacts`.
const SNAPSHOT_REL: &str = "contacts/apple-contacts/contacts.jsonl";

/// Seconds between contacts passes. Address books churn slowly; a pass that
/// finds nothing changed is one cheap in-process enumeration — every 6 hours
/// is generous freshness (the brief's "~6h").
pub const APPLE_CONTACTS_SYNC_SECS: u64 = 6 * 3600;

/// How long a TCC-prompt answer is awaited before deferring to the next pass
/// (an already-decided request resolves instantly). Mirrors eventkit.
const ACCESS_WAIT_SECS: u64 = 5;

/// Authorization to read Contacts, as the collector cares about it. A local
/// mirror of [`crate::eventkit::AuthStatus`] (kept separate so the two TCC
/// bridges don't couple).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    NotDetermined,
    Denied,
    Granted,
}

impl AuthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthStatus::NotDetermined => "not-determined",
            AuthStatus::Denied => "denied",
            AuthStatus::Granted => "granted",
        }
    }
}

/// Current Contacts authorization for this process.
pub fn auth_status() -> AuthStatus {
    imp::auth_status()
}

/// Ask for Contacts access (fires the TCC prompt when not-determined and the
/// process carries `NSContactsUsageDescription`). Waits up to `wait_secs` for
/// the answer; a live prompt usually outlives the wait, in which case the
/// caller skips this pass and picks the grant up on the next one.
pub fn request_access(wait_secs: u64) -> bool {
    imp::request_access(wait_secs)
}

/// Enumerate every unified contact, each as a [`RawAppleContact`] (raw
/// strings, no normalization — [`map_contact`] does that). macOS only;
/// elsewhere it returns empty.
pub fn fetch_contacts() -> Result<Vec<RawAppleContact>> {
    imp::fetch_contacts()
}

// ---------------------------------------------------------------------------
// The plain-Rust intermediate: what the FFI extracts from each CNContact,
// before any normalization. This is the seam that makes mapping testable —
// `map_contact` consumes it with zero Apple types in scope.

/// A year-optional calendar date (`CNContactBirthdayKey` / a `CNContactDates`
/// entry). A year-less birthday has `year == None`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDate {
    pub year: Option<i32>,
    pub month: i32,
    pub day: i32,
}

/// One labeled value (`CNLabeledValue`): a localized label (may be empty) and
/// its string value. Used for postal addresses (value pre-formatted), urls,
/// social profiles, and custom dates.
#[derive(Debug, Clone, PartialEq)]
pub struct RawLabeled {
    pub label: String,
    pub value: String,
}

/// One `CNContact`, flattened to plain Rust by the FFI. Strings are verbatim
/// (CNContact returns "" for absent scalar fields, never null); arrays are in
/// source order. No normalization, no dedup — all of that lives in the pure
/// [`map_contact`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawAppleContact {
    /// `CNContact.identifier` — stable across syncs; the dedupe key / `id`.
    pub identifier: String,
    pub given: String,
    pub family: String,
    /// `CNContact.displayName` is not a property; the FFI passes whatever name
    /// formatting it computed (empty if none).
    pub display_name: String,
    pub org: String,
    pub title: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub postal: Vec<RawLabeled>,
    pub urls: Vec<RawLabeled>,
    pub social: Vec<RawLabeled>,
    pub birthday: Option<RawDate>,
    /// Labeled anniversaries / custom dates (`CNContactDatesKey`).
    pub dates: Vec<(String, RawDate)>,
    pub note: String,
}

// ---------------------------------------------------------------------------
// The pure mapping core: RawAppleContact → contacts::Contact. Cross-platform,
// no Apple types, fully unit-tested. Mirrors google_contacts' normalize_person
// discipline: consume what maps to the normalized columns, park everything
// else in `extra` at full fidelity.

/// Lowercase + trim emails, drop empties and exact dups (source order). The
/// same shape as google_contacts' `value_strings`, specialized per-handle.
fn norm_emails(raw: &[String]) -> Vec<String> {
    dedup_nonempty(raw.iter().map(|e| normalize_email(e)))
}

/// E.164 phones where derivable, verbatim otherwise (no region hint — Apple
/// reports no per-number region, so a national number stays as typed and only
/// `+`-prefixed numbers reach E.164: the correct, no-guess behavior).
fn norm_phones(raw: &[String]) -> Vec<String> {
    dedup_nonempty(raw.iter().map(|p| normalize_phone(p, None)))
}

fn dedup_nonempty(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in it {
        if !s.is_empty() && !out.iter().any(|e| e == &s) {
            out.push(s);
        }
    }
    out
}

/// A [`RawDate`] → the contract's `{month, day[, year]}` object. A year-less
/// birthday omits the `year` key entirely (matching the contract example
/// `{"date":{"month":3,"day":14}}`); a dated one includes it.
fn date_obj(d: &RawDate) -> Value {
    let mut m = Map::new();
    if let Some(y) = d.year {
        m.insert("year".into(), Value::from(y));
    }
    m.insert("month".into(), Value::from(d.month));
    m.insert("day".into(), Value::from(d.day));
    Value::Object(m)
}

/// One `{label, value}` object (labeled urls / social / postal in `extra`).
fn labeled_obj(l: &RawLabeled) -> Value {
    let mut m = Map::new();
    if !l.label.is_empty() {
        m.insert("label".into(), Value::from(l.label.clone()));
    }
    m.insert("value".into(), Value::from(l.value.clone()));
    Value::Object(m)
}

/// Map one extracted Apple contact to the normalized cross-source
/// [`Contact`]. **The unit-tested core.** Names/emails/phones/orgs fill the
/// columns; birthdays, dates, addresses, urls, social profiles, and the note
/// ride verbatim in `extra` (full fidelity). `photo` is omitted by design:
/// the contract stores real photo URLs only, and CNContact gives image *data*
/// — inlining a base64 blob would violate it.
pub fn map_contact(raw: RawAppleContact) -> Contact {
    let emails = norm_emails(&raw.emails);
    let phones = norm_phones(&raw.phones);

    // Display name: prefer Apple's formatted name; fall back to "given family"
    // assembled from the parts so a contact with only structured names still
    // shows one.
    let name = if !raw.display_name.trim().is_empty() {
        raw.display_name.trim().to_string()
    } else {
        format!("{} {}", raw.given.trim(), raw.family.trim()).trim().to_string()
    };

    let orgs = match (raw.org.trim(), raw.title.trim()) {
        ("", "") => Vec::new(),
        (org, title) => vec![ContactOrg {
            name: (!org.is_empty()).then(|| org.to_string()),
            title: (!title.is_empty()).then(|| title.to_string()),
        }],
    };

    // --- extra: everything the columns don't carry, verbatim. ---
    let mut extra = Map::new();

    // Birthday → extra.birthdays = [{date: {month, day[, year]}}], matching
    // the contract example. A list (not a scalar) so it shares a shape with
    // Google's birthdays and leaves room for an alt/lunar birthday later.
    if let Some(b) = &raw.birthday {
        extra.insert(
            "birthdays".into(),
            Value::Array(vec![{
                let mut m = Map::new();
                m.insert("date".into(), date_obj(b));
                Value::Object(m)
            }]),
        );
    }

    // Labeled anniversaries / custom dates → extra.dates = [{label, date}].
    if !raw.dates.is_empty() {
        extra.insert(
            "dates".into(),
            Value::Array(
                raw.dates
                    .iter()
                    .map(|(label, d)| {
                        let mut m = Map::new();
                        if !label.is_empty() {
                            m.insert("label".into(), Value::from(label.clone()));
                        }
                        m.insert("date".into(), date_obj(d));
                        Value::Object(m)
                    })
                    .collect(),
            ),
        );
    }

    insert_labeled(&mut extra, "addresses", &raw.postal);
    insert_labeled(&mut extra, "urls", &raw.urls);
    insert_labeled(&mut extra, "social", &raw.social);

    if !raw.note.trim().is_empty() {
        extra.insert("note".into(), Value::from(raw.note.clone()));
    }

    Contact {
        source: SOURCE.to_string(),
        id: raw.identifier,
        // One federated store, no per-account split — account stays empty.
        account: String::new(),
        name,
        given: raw.given.trim().to_string(),
        family: raw.family.trim().to_string(),
        emails,
        phones,
        orgs,
        // TODO(apple-contacts photo): CNContact exposes image DATA, not a URL;
        // the contract stores real URLs only, so v1 omits the photo rather
        // than inline a base64 blob. Revisit if we add a vault blob store.
        photo: String::new(),
        // Apple contacts are user-saved (or saved by a synced account), never
        // the auto-collected interaction graph that `other` marks.
        other: false,
        // CNContact carries no reliable per-contact modified stamp in the
        // fetched key set; leave it unset rather than guess.
        updated: None,
        extra,
    }
}

/// Insert a non-empty list of labeled values under `key`, dropping entries
/// whose value normalized away to empty.
fn insert_labeled(extra: &mut Map<String, Value>, key: &str, items: &[RawLabeled]) {
    let arr: Vec<Value> = items
        .iter()
        .filter(|l| !l.value.trim().is_empty())
        .map(labeled_obj)
        .collect();
    if !arr.is_empty() {
        extra.insert(key.into(), Value::Array(arr));
    }
}

// ---------------------------------------------------------------------------
// DEF wiring.

/// Hourly-ish pass: enumerate (TCC-gated), map, snapshot. Silent when nothing
/// changed; a permission denial is a quiet no-op (the card surfaces the
/// grant), never a logged error — the calendar/Safari-history precedent.
fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    use crate::registry::CollectOutcome;
    let s = vault.collect_apple_contacts()?;
    Ok(CollectOutcome::note_if(s.changed > 0, || {
        format!("apple contacts synced — {} changes ({} total)", s.changed, s.total)
    }))
}

fn def_permission() -> PermissionInfo {
    PermissionInfo {
        kind: "contacts",
        granted: Some(auth_status() == AuthStatus::Granted),
        required: true,
    }
}

/// Snapshot last-data probe — the file's mtime, mirroring google_contacts'
/// snapshot-style hook (a single-file snapshot, so probe the file directly).
fn def_last_data(vault: &Vault) -> Option<String> {
    let path = vault.resolve(SNAPSHOT_REL).ok()?;
    crate::registry::file_mtime(&path)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-contacts",
        name: "Apple Contacts",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your macOS Contacts address book via the system Contacts \
                      framework. One permission prompt federates iCloud, Google, Exchange, \
                      and CardDAV contacts into one deduped store; birthdays, anniversaries, \
                      addresses, and notes ride along.",
        domain: "contacts",
        vault_path: "contacts/apple-contacts/",
        toggleable: true,
        setup: &[
            "Approve the Contacts access prompt the first time the app (and separately the troved daemon) asks.",
            "If the prompt was declined: System Settings → Privacy & Security → Contacts → enable Trove and troved.",
            "iCloud / Google / Exchange / CardDAV contacts are covered automatically once the account is added to macOS with Contacts on — they all federate into one store.",
        ],
        caveats: "There is no manual-grant path until the app has asked once: the Contacts settings pane only lists apps after they request access. Contact photos are not stored in this version.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(APPLE_CONTACTS_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(pull),
};

/// One contacts sync pass, for logging / the UI notice.
#[derive(Debug, Clone, Default)]
pub struct AppleContactsSyncStats {
    /// Contacts added, updated, or removed this pass.
    pub changed: u64,
    /// Contacts in the snapshot after the pass.
    pub total: u64,
    /// True when the pass was skipped for lack of TCC access.
    pub skipped_permission: bool,
}

impl Vault {
    /// One Apple Contacts sync pass: TCC gate → enumerate the unified store →
    /// map each contact → rewrite the snapshot if it changed. Silently a
    /// no-op while access is denied (returns `skipped_permission`). The map +
    /// snapshot half is OS-free; the enumeration lives in [`fetch_contacts`].
    pub fn collect_apple_contacts(&self) -> Result<AppleContactsSyncStats> {
        match auth_status() {
            AuthStatus::Granted => {}
            AuthStatus::NotDetermined => {
                if !request_access(ACCESS_WAIT_SECS) {
                    return Ok(AppleContactsSyncStats {
                        skipped_permission: true,
                        ..Default::default()
                    });
                }
            }
            AuthStatus::Denied => {
                return Ok(AppleContactsSyncStats {
                    skipped_permission: true,
                    ..Default::default()
                })
            }
        }
        let raw = fetch_contacts()?;
        self.apply_apple_contacts(raw)
    }

    /// Map a freshly-fetched raw set and commit it as the snapshot. Split out
    /// from [`Vault::collect_apple_contacts`] so the map+diff+write half is
    /// testable with injected data (no FFI). Sorted by `id` for clean diffs;
    /// the file is rewritten only when its contents changed.
    fn apply_apple_contacts(&self, raw: Vec<RawAppleContact>) -> Result<AppleContactsSyncStats> {
        let mut next: Vec<Contact> = raw.into_iter().map(map_contact).collect();
        next.sort_by(|a, b| a.id.cmp(&b.id));

        let existing: Vec<Contact> = self.read_snapshot(SNAPSHOT_REL)?;
        let changed = snapshot_changes(&existing, &next);
        let total = next.len() as u64;

        // Write on any content change, and create the file on the first
        // successful pass even if empty (its existence registers the store) —
        // but never rewrite an unchanged snapshot (keeps the mtime, so the
        // last-data probe and git diffs stay quiet).
        let exists = self.resolve(SNAPSHOT_REL).map(|p| p.exists()).unwrap_or(false);
        if changed > 0 || !exists {
            self.write_snapshot(SNAPSHOT_REL, &next)?;
        }
        Ok(AppleContactsSyncStats { changed, total, skipped_permission: false })
    }
}

/// How many contacts differ between the old and new snapshots: additions,
/// removals, and field changes, keyed by `id`. Pure — drives the change count
/// and the "is a rewrite needed" decision.
fn snapshot_changes(old: &[Contact], new: &[Contact]) -> u64 {
    use std::collections::BTreeMap;
    let old_by: BTreeMap<&str, &Contact> = old.iter().map(|c| (c.id.as_str(), c)).collect();
    let new_by: BTreeMap<&str, &Contact> = new.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut changed = 0u64;
    for (id, c) in &new_by {
        match old_by.get(id) {
            Some(before) if *before == *c => {}
            _ => changed += 1, // added or field-changed
        }
    }
    for id in old_by.keys() {
        if !new_by.contains_key(id) {
            changed += 1; // removed
        }
    }
    changed
}

/// [`crate::registry::IntegrationDef::pull`] adapter — the manual "Sync now" /
/// first-grant path. Blocking (enumerates the store).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.collect_apple_contacts()?;
    let headline = if s.skipped_permission {
        "Contacts permission needed".to_string()
    } else if s.changed > 0 {
        format!("{} contact changes ({} total)", s.changed, s.total)
    } else {
        format!("contacts up to date ({} total)", s.total)
    };
    Ok(PullOutcome {
        headline,
        counts: std::collections::BTreeMap::from([
            ("changed", s.changed),
            ("total", s.total),
        ]),
    })
}

// ---------------------------------------------------------------------------
// macOS FFI: CNContactStore → Vec<RawAppleContact>. Thin — it only extracts
// raw strings; all meaning lives in the pure map_contact. Validated live (the
// TCC-gated Needs-David), not in tests.

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{Bool, ProtocolObject};
    use objc2::AnyThread;
    use objc2_contacts::{
        CNAuthorizationStatus, CNContact, CNContactBirthdayKey, CNContactDatesKey,
        CNContactEmailAddressesKey, CNContactFamilyNameKey, CNContactFetchRequest,
        CNContactGivenNameKey, CNContactJobTitleKey, CNContactNoteKey,
        CNContactOrganizationNameKey, CNContactPhoneNumbersKey, CNContactPostalAddressesKey,
        CNContactSocialProfilesKey, CNContactStore, CNContactUrlAddressesKey, CNEntityType,
        CNKeyDescriptor, CNLabeledValue, CNPhoneNumber, CNPostalAddress, CNSocialProfile,
    };
    use objc2_foundation::{NSArray, NSDateComponents, NSError, NSString};

    use super::{AuthStatus, RawAppleContact, RawDate, RawLabeled};

    pub fn auth_status() -> AuthStatus {
        let status =
            unsafe { CNContactStore::authorizationStatusForEntityType(CNEntityType::Contacts) };
        match status {
            CNAuthorizationStatus::NotDetermined => AuthStatus::NotDetermined,
            CNAuthorizationStatus::Authorized => AuthStatus::Granted,
            // Restricted / Denied / (any future limited mode): no full read
            // without user action in System Settings.
            _ => AuthStatus::Denied,
        }
    }

    pub fn request_access(wait_secs: u64) -> bool {
        let store = unsafe { CNContactStore::new() };
        let (tx, rx) = mpsc::channel::<bool>();
        let block = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
            let _ = tx.send(granted.as_bool());
        });
        unsafe {
            store.requestAccessForEntityType_completionHandler(CNEntityType::Contacts, &block);
        }
        rx.recv_timeout(Duration::from_secs(wait_secs)).unwrap_or(false)
    }

    /// A `Retained<NSString>` → owned `String` (the getters return owned
    /// retained strings; CNContact yields "" for absent scalars, never null).
    fn s(v: Retained<NSString>) -> String {
        v.to_string()
    }

    /// An `Option<Retained<NSString>>` (a labeled value's label) → `String`.
    fn os(v: Option<Retained<NSString>>) -> String {
        v.map(|x| x.to_string()).unwrap_or_default()
    }

    /// The string `value`s of a `CNLabeledValue<NSString>` array (emails, urls).
    fn string_values(arr: &NSArray<CNLabeledValue<NSString>>) -> Vec<String> {
        arr.iter().map(|lv| s(unsafe { lv.value() })).collect()
    }

    /// `{label, value}` of a `CNLabeledValue<NSString>` array.
    fn labeled_strings(arr: &NSArray<CNLabeledValue<NSString>>) -> Vec<RawLabeled> {
        arr.iter()
            .map(|lv| RawLabeled {
                label: os(unsafe { lv.label() }),
                value: s(unsafe { lv.value() }),
            })
            .collect()
    }

    /// One `NSDateComponents` → a [`RawDate`], or `None` when month/day are
    /// undefined. `NSDateComponentUndefined` (`NSIntegerMax`) marks an unset
    /// field; a year-less birthday sets only month/day, so a year outside a
    /// sane calendar range is treated as absent.
    fn raw_date(dc: &NSDateComponents) -> Option<RawDate> {
        let (y, m, d) = (dc.year(), dc.month(), dc.day());
        let valid = |v: isize| v != isize::MAX && v > 0;
        if !valid(m) || !valid(d) {
            return None;
        }
        // A real Gregorian year is well below the undefined sentinel; anything
        // implausible (sentinel, <=0, absurdly large) means "no year".
        let year = (valid(y) && (1..=9999).contains(&y)).then_some(y as i32);
        Some(RawDate { year, month: m as i32, day: d as i32 })
    }

    /// The key set passed to the fetch request — exactly the fields
    /// [`super::map_contact`] consumes. Each `CNContact*Key` is an `unsafe
    /// static &NSString`; `NSString` conforms to `CNKeyDescriptor`, so each is
    /// viewed as a `&ProtocolObject<dyn CNKeyDescriptor>` and packed into an
    /// `NSArray`. `identifier` is always returned without being requested, so
    /// it is not in the set.
    fn keys_to_fetch() -> Retained<NSArray<ProtocolObject<dyn CNKeyDescriptor>>> {
        // SAFETY: these statics are non-null NSString key constants; reading
        // them and viewing NSString as its CNKeyDescriptor conformance is the
        // documented usage.
        let keys: [&ProtocolObject<dyn CNKeyDescriptor>; 12] = unsafe {
            [
                ProtocolObject::from_ref(CNContactGivenNameKey),
                ProtocolObject::from_ref(CNContactFamilyNameKey),
                ProtocolObject::from_ref(CNContactOrganizationNameKey),
                ProtocolObject::from_ref(CNContactJobTitleKey),
                ProtocolObject::from_ref(CNContactEmailAddressesKey),
                ProtocolObject::from_ref(CNContactPhoneNumbersKey),
                ProtocolObject::from_ref(CNContactPostalAddressesKey),
                ProtocolObject::from_ref(CNContactUrlAddressesKey),
                ProtocolObject::from_ref(CNContactSocialProfilesKey),
                ProtocolObject::from_ref(CNContactBirthdayKey),
                ProtocolObject::from_ref(CNContactDatesKey),
                ProtocolObject::from_ref(CNContactNoteKey),
            ]
        };
        NSArray::from_slice(&keys)
    }

    /// Extract one `CNContact` into the plain-Rust intermediate. All meaning
    /// (normalization, extra-parking) lives in [`super::map_contact`]; this is
    /// pure extraction.
    fn extract(c: &CNContact) -> RawAppleContact {
        unsafe {
            // Postal addresses: format each CNPostalAddress to a single string
            // via CNPostalAddressFormatter-equivalent — here we keep the
            // street/city/state/postal/country joined by the framework's
            // `mailingAddress`-style value. The labeled `value` is the
            // formatted address; sub-fields would over-structure `extra`.
            let postal: Vec<RawLabeled> = c
                .postalAddresses()
                .iter()
                .map(|lv| RawLabeled {
                    label: os(lv.label()),
                    value: postal_string(&lv.value()),
                })
                .collect();

            let social: Vec<RawLabeled> = c
                .socialProfiles()
                .iter()
                .map(|lv| {
                    let p: Retained<CNSocialProfile> = lv.value();
                    // Prefer the profile URL; fall back to "service:username".
                    let url = s(p.urlString());
                    let value = if !url.is_empty() {
                        url
                    } else {
                        let service = s(p.service());
                        let user = s(p.username());
                        match (service.is_empty(), user.is_empty()) {
                            (false, false) => format!("{service}:{user}"),
                            (true, false) => user,
                            _ => service,
                        }
                    };
                    RawLabeled { label: os(lv.label()), value }
                })
                .collect();

            let phones: Vec<String> = c
                .phoneNumbers()
                .iter()
                .map(|lv| {
                    let n: Retained<CNPhoneNumber> = lv.value();
                    s(n.stringValue())
                })
                .collect();

            let dates: Vec<(String, RawDate)> = c
                .dates()
                .iter()
                .filter_map(|lv| {
                    let label = os(lv.label());
                    raw_date(&lv.value()).map(|d| (label, d))
                })
                .collect();

            RawAppleContact {
                identifier: s(c.identifier()),
                given: s(c.givenName()),
                family: s(c.familyName()),
                // CNContact has no `displayName` property in the fetched set;
                // assemble from parts in map_contact instead (display_name
                // stays empty here, the documented fallback path).
                display_name: String::new(),
                org: s(c.organizationName()),
                title: s(c.jobTitle()),
                emails: string_values(&c.emailAddresses()),
                phones,
                postal,
                urls: labeled_strings(&c.urlAddresses()),
                social,
                birthday: c.birthday().and_then(|dc| raw_date(&dc)),
                dates,
                note: s(c.note()),
            }
        }
    }

    /// Join a CNPostalAddress's components into one human address string
    /// (street, city, state postalCode, country), skipping empty parts.
    fn postal_string(a: &CNPostalAddress) -> String {
        let part = |v: Retained<NSString>| {
            let v = v.to_string();
            (!v.trim().is_empty()).then_some(v)
        };
        unsafe {
            [
                part(a.street()),
                part(a.city()),
                part(a.state()),
                part(a.postalCode()),
                part(a.country()),
            ]
        }
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    }

    pub fn fetch_contacts() -> Result<Vec<RawAppleContact>> {
        let store = unsafe { CNContactStore::new() };
        let keys = keys_to_fetch();
        let request = unsafe {
            CNContactFetchRequest::initWithKeysToFetch(CNContactFetchRequest::alloc(), &keys)
        };

        // The enumeration runs the block synchronously on this thread; collect
        // into a RefCell, then take it back out once the block is dropped.
        let collected: std::cell::RefCell<Vec<RawAppleContact>> =
            std::cell::RefCell::new(Vec::new());
        let mut err: Option<Retained<NSError>> = None;
        let ok = {
            let block = RcBlock::new(
                |contact: std::ptr::NonNull<CNContact>, _stop: std::ptr::NonNull<Bool>| {
                    // Each contact is autoreleased per iteration; drain the
                    // pool so a large address book doesn't balloon memory.
                    autoreleasepool(|_| {
                        let c = unsafe { contact.as_ref() };
                        collected.borrow_mut().push(extract(c));
                    });
                },
            );
            unsafe {
                store.enumerateContactsWithFetchRequest_error_usingBlock(
                    &request,
                    Some(&mut err),
                    &block,
                )
            }
        };
        if !ok {
            let msg = err
                .map(|e| e.localizedDescription().to_string())
                .unwrap_or_else(|| "CNContactStore enumeration failed".to_string());
            return Err(anyhow!("apple contacts: {msg}"));
        }
        Ok(collected.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Non-macOS stub: the crate must build everywhere. Contacts is macOS-only, so
// status is Denied and the fetch yields nothing — exactly the eventkit shape.

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::Result;

    use super::{AuthStatus, RawAppleContact};

    pub fn auth_status() -> AuthStatus {
        AuthStatus::Denied
    }

    pub fn request_access(_wait_secs: u64) -> bool {
        false
    }

    pub fn fetch_contacts() -> Result<Vec<RawAppleContact>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-acontacts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn date(year: Option<i32>, month: i32, day: i32) -> RawDate {
        RawDate { year, month, day }
    }

    fn labeled(label: &str, value: &str) -> RawLabeled {
        RawLabeled { label: label.into(), value: value.into() }
    }

    /// A full, rich contact exercising every mapped column and extra key.
    fn rich() -> RawAppleContact {
        RawAppleContact {
            identifier: "ABC-123".into(),
            given: "Alice".into(),
            family: "Example".into(),
            display_name: "Alice Example".into(),
            org: "Example Corp".into(),
            title: "CTO".into(),
            emails: vec!["Alice@Example.com".into(), " alice@work.com ".into()],
            // One international (→ E.164), one national (verbatim, no region).
            phones: vec!["+1 (415) 555-0142".into(), "(020) 7946 0958".into()],
            postal: vec![labeled("home", "1 Main St, Springfield, IL, 62704, USA")],
            urls: vec![labeled("homepage", "https://alice.example")],
            social: vec![labeled("Twitter", "https://twitter.com/alice")],
            birthday: Some(date(Some(1990), 3, 14)),
            dates: vec![("anniversary".into(), date(Some(2015), 6, 20))],
            note: "Old friend from school.".into(),
        }
    }

    #[test]
    fn full_contact_maps_core_and_parks_rest_in_extra() {
        let c = map_contact(rich());
        assert_eq!(c.source, "apple-contacts");
        assert_eq!(c.id, "ABC-123", "id is the CNContact identifier");
        assert!(c.account.is_empty(), "one federated store — no account");
        assert_eq!(c.name, "Alice Example");
        assert_eq!(c.given, "Alice");
        assert_eq!(c.family, "Example");
        // Lowercased + trimmed.
        assert_eq!(c.emails, vec!["alice@example.com", "alice@work.com"]);
        // International → E.164; national stays verbatim (no region hint).
        assert_eq!(c.phones, vec!["+14155550142", "(020) 7946 0958"]);
        assert_eq!(c.orgs.len(), 1);
        assert_eq!(c.orgs[0].name.as_deref(), Some("Example Corp"));
        assert_eq!(c.orgs[0].title.as_deref(), Some("CTO"));
        // Photo always omitted in v1 (CNContact gives data, not a URL).
        assert!(c.photo.is_empty());
        assert!(!c.other, "apple contacts are user-saved");
        // note → extra.note (verbatim).
        assert_eq!(c.extra.get("note").and_then(Value::as_str), Some("Old friend from school."));
        // addresses / urls / social all parked.
        assert!(c.extra.contains_key("addresses"));
        assert!(c.extra.contains_key("urls"));
        assert!(c.extra.contains_key("social"));
        assert_eq!(
            c.extra["urls"],
            json!([{"label": "homepage", "value": "https://alice.example"}])
        );
    }

    #[test]
    fn yearless_birthday_omits_the_year_key() {
        let mut raw = rich();
        raw.birthday = Some(date(None, 3, 14));
        let c = map_contact(raw);
        // Exactly the contract example shape: {"date":{"month":3,"day":14}}.
        assert_eq!(c.extra["birthdays"], json!([{"date": {"month": 3, "day": 14}}]));
        // And no "year" key leaked in.
        let bd = &c.extra["birthdays"][0]["date"];
        assert!(bd.get("year").is_none(), "year-less birthday must omit year");
    }

    #[test]
    fn dated_birthday_includes_the_year() {
        let c = map_contact(rich()); // birthday is 1990-03-14
        assert_eq!(c.extra["birthdays"], json!([{"date": {"year": 1990, "month": 3, "day": 14}}]));
    }

    #[test]
    fn custom_labeled_anniversary_lands_in_extra_dates() {
        let c = map_contact(rich());
        assert_eq!(
            c.extra["dates"],
            json!([{"label": "anniversary", "date": {"year": 2015, "month": 6, "day": 20}}])
        );
    }

    #[test]
    fn emails_are_lowercased_and_deduped() {
        let mut raw = rich();
        raw.emails = vec![
            "Bob@Example.com".into(),
            "bob@example.com".into(), // dup after lowercasing → dropped
            "BOB@EXAMPLE.COM".into(), // dup again → dropped
            " bob@other.com ".into(), // trimmed, kept
        ];
        let c = map_contact(raw);
        assert_eq!(c.emails, vec!["bob@example.com", "bob@other.com"]);
    }

    #[test]
    fn sparse_email_only_contact_maps_minimally() {
        let raw = RawAppleContact {
            identifier: "X1".into(),
            emails: vec!["only@example.com".into()],
            ..Default::default()
        };
        let c = map_contact(raw);
        // The omit-empty contract: a sparse line is just source + id + emails.
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(
            v,
            json!({"source": "apple-contacts", "id": "X1", "emails": ["only@example.com"]})
        );
    }

    #[test]
    fn name_falls_back_to_given_family_when_no_display_name() {
        let raw = RawAppleContact {
            identifier: "X2".into(),
            given: "Carol".into(),
            family: "Danvers".into(),
            ..Default::default()
        };
        let c = map_contact(raw);
        assert_eq!(c.name, "Carol Danvers");
    }

    #[test]
    fn snapshot_write_and_read_round_trips_through_contact() {
        let v = temp_vault("snapshot");
        let stats = v
            .apply_apple_contacts(vec![rich(), {
                let mut other = rich();
                other.identifier = "AAA-000".into(); // sorts before ABC-123
                other.display_name = "Aaron".into();
                other
            }])
            .unwrap();
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.total, 2);

        let loaded: Vec<Contact> = v.read_snapshot(SNAPSHOT_REL).unwrap();
        // Sorted by id for clean diffs.
        let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["AAA-000", "ABC-123"]);
        // Full fidelity survived the JSONL round-trip.
        let alice = loaded.iter().find(|c| c.id == "ABC-123").unwrap();
        assert_eq!(alice.emails, vec!["alice@example.com", "alice@work.com"]);
        assert_eq!(alice.extra["birthdays"], json!([{"date": {"year": 1990, "month": 3, "day": 14}}]));
    }

    #[test]
    fn unchanged_snapshot_is_not_rewritten() {
        let v = temp_vault("nochange");
        v.apply_apple_contacts(vec![rich()]).unwrap();
        let path = v.root().join(SNAPSHOT_REL);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let stats = v.apply_apple_contacts(vec![rich()]).unwrap();
        assert_eq!(stats.changed, 0, "identical set is no change");
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            mtime,
            "unchanged snapshot keeps its mtime"
        );
    }

    #[test]
    fn changes_count_adds_removes_and_edits() {
        let v = temp_vault("changes");
        v.apply_apple_contacts(vec![rich()]).unwrap(); // ABC-123

        let mut edited = rich();
        edited.given = "Alicia".into();
        edited.display_name = "Alicia Example".into();
        let added = RawAppleContact {
            identifier: "NEW-1".into(),
            given: "New".into(),
            ..Default::default()
        };
        // ABC-123 edited, NEW-1 added; nothing else → 2 changes.
        let stats = v.apply_apple_contacts(vec![edited, added]).unwrap();
        assert_eq!(stats.changed, 2);

        // Now drop both: 2 removals.
        let empty: Vec<RawAppleContact> = Vec::new();
        let stats = v.apply_apple_contacts(empty).unwrap();
        assert_eq!(stats.changed, 2);
        assert_eq!(stats.total, 0);
    }

    #[test]
    fn empty_org_and_title_yield_no_orgs() {
        let raw = RawAppleContact {
            identifier: "X3".into(),
            given: "No".into(),
            family: "Org".into(),
            ..Default::default()
        };
        assert!(map_contact(raw).orgs.is_empty());
    }

    #[test]
    fn old_contact_line_still_deserializes() {
        // Back-compat: a minimal historical line (just source+id) parses, and
        // a line written by this collector parses back unchanged.
        let minimal: Contact = serde_json::from_str(r#"{"source":"apple-contacts","id":"OLD"}"#).unwrap();
        assert_eq!(minimal.id, "OLD");
        assert!(minimal.emails.is_empty() && minimal.extra.is_empty());

        let line = serde_json::to_string(&map_contact(rich())).unwrap();
        let back: Contact = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, "ABC-123");
    }
}
