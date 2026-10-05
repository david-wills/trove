//! Anti-drift harness for the public vault spec (`docs/vault-spec/`).
//!
//! Three representations of every contract must agree, forever:
//! 1. the Rust types collectors compile against,
//! 2. the JSON Schemas third parties validate against,
//! 3. the example lines in the spec pages (which are the *same bytes* as
//!    the fixture files here — a doc-sync check keeps them verbatim).
//!
//! A contract change that updates one without the others fails this suite.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use trove_core::{
    ActivityEvent, AdRecord, Almanac, BrowserVisit, CalendarChange, CalendarOccurrence, Checkin,
    Contact, EnvGeoEvent, EnvReading, Fix, Habit, Highlight, HomeReading, Item, LineItem,
    MediaItem, Meeting, Message, Note, NutritionEntry, Observation, Photo, Post, Recording,
    Search, Segment, SleepSession, Task, TaskEvent, TimeEntry, DOMAINS,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spec").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn schema(name: &str) -> jsonschema::Validator {
    let path = repo_root().join("docs/vault-spec/schemas").join(name);
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    jsonschema::validator_for(&serde_json::from_str(&raw).unwrap()).unwrap()
}

fn errors(v: &jsonschema::Validator, value: &Value) -> Vec<String> {
    v.iter_errors(value).map(|e| e.to_string()).collect()
}

/// Every fixture line must (a) validate against the schema, (b) deserialize
/// into the Rust type, and (c) still validate after a serde round-trip — so
/// omit-empty serialization can never drop a spec-required field.
fn check<T: serde::de::DeserializeOwned + serde::Serialize>(fixture_name: &str, schema_name: &str) {
    let v = schema(schema_name);
    for (i, line) in fixture(fixture_name).lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert!(
            v.is_valid(&value),
            "{fixture_name}:{}: schema errors {:?}",
            i + 1,
            errors(&v, &value)
        );
        let typed: T = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{fixture_name}:{}: serde rejects spec example: {e}", i + 1));
        let re = serde_json::to_value(&typed).unwrap();
        assert!(
            v.is_valid(&re),
            "{fixture_name}:{}: re-serialized form fails schema: {:?}",
            i + 1,
            errors(&v, &re)
        );
    }
}

/// Phase 3 contract DRAFTS validate against their schema only — no Rust-type
/// round-trip. Unlike the four ratified contracts, a draft is not yet bound to
/// a Rust type in the shipping crate or registered in `DOMAINS`; that binding
/// lands when Phase 4 builds the domain's first collector (ratification is
/// David's gate). Until then a draft holds two reviewable invariants: every
/// example validates against its schema (here), and the spec-page example
/// block is the fixture verbatim (`assert_doc_block`).
fn check_schema_only(fixture_name: &str, schema_name: &str) {
    let v = schema(schema_name);
    for (i, line) in fixture(fixture_name).lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{fixture_name}:{}: invalid JSON: {e}", i + 1));
        assert!(
            v.is_valid(&value),
            "{fixture_name}:{}: schema errors {:?}",
            i + 1,
            errors(&v, &value)
        );
    }
}

/// A spec page's first ```{tag} fenced block must equal the named fixture
/// verbatim (trailing whitespace ignored) — the example lines authors paste
/// into a spec page are the exact bytes collectors are tested against. Shared
/// by the ratified-contract and Phase-3-draft doc-sync checks.
fn assert_doc_block(page: &str, tag: &str, fixture_name: &str) {
    let path = repo_root().join("docs/vault-spec").join(page);
    let doc = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let fence = format!("```{tag}\n");
    let start =
        doc.find(&fence).unwrap_or_else(|| panic!("{page}: no ```{tag} block")) + fence.len();
    let end = doc[start..].find("```").unwrap() + start;
    assert_eq!(
        doc[start..end].trim_end(),
        fixture(fixture_name).trim_end(),
        "{page}'s ```{tag} example must equal tests/fixtures/spec/{fixture_name} verbatim"
    );
}

#[test]
fn fixtures_validate_against_schemas_and_rust_types() {
    check::<Message>("correspondence.message.jsonl", "correspondence.message.schema.json");
    check::<Task>("tasks.task.jsonl", "tasks.task.schema.json");
    check::<TaskEvent>("tasks.event.jsonl", "tasks.event.schema.json");
    check::<MediaItem>("media.play.jsonl", "media.play.schema.json");
    check::<CalendarOccurrence>("calendar.occurrence.jsonl", "calendar.occurrence.schema.json");
    check::<CalendarChange>("calendar.change.jsonl", "calendar.change.schema.json");
    check::<Contact>("contacts.contact.jsonl", "contacts.contact.schema.json");
    check::<EnvReading>("environment.reading.jsonl", "environment.reading.schema.json");
    check::<EnvGeoEvent>("environment.geo-event.jsonl", "environment.geo-event.schema.json");
    check::<Almanac>("environment.almanac.jsonl", "environment.almanac.schema.json");
    check::<Recording>("voice.recording.jsonl", "voice.recording.schema.json");
    check::<Note>("notes.note.jsonl", "notes.note.schema.json");
    check::<Post>("social.post.jsonl", "social.post.schema.json");
    check::<Meeting>("meetings.meeting.jsonl", "meetings.meeting.schema.json");
    check::<Photo>("photos.photo.jsonl", "photos.photo.schema.json");
    check::<Item>("reading.item.jsonl", "reading.item.schema.json");
    check::<Highlight>("reading.highlight.jsonl", "reading.highlight.schema.json");
    check::<HomeReading>("home.reading.jsonl", "home.reading.schema.json");
    check::<Habit>("habits.habit.jsonl", "habits.habit.schema.json");
    check::<Checkin>("habits.checkin.jsonl", "habits.checkin.schema.json");
    check::<TimeEntry>("time-entries.entry.jsonl", "time-entries.entry.schema.json");
    check::<LineItem>("finance-purchases.line-item.jsonl", "finance-purchases.line-item.schema.json");
    check::<NutritionEntry>("health-nutrition.entry.jsonl", "health-nutrition.entry.schema.json");
    check::<Observation>("health-medical.observation.jsonl", "health-medical.observation.schema.json");
    check::<SleepSession>("health-sleep.session.jsonl", "health-sleep.session.schema.json");
    check::<Segment>("travel.segment.jsonl", "travel.segment.schema.json");
    check::<Fix>("location.fix.jsonl", "location.fix.schema.json");
    check::<Search>("browser-searches.search.jsonl", "browser-searches.search.schema.json");
    // The three streams the external collector owns (S4, 2026-09-16): the
    // spec page is the only contract between it and this reader.
    check::<ActivityEvent>("activity.event.jsonl", "activity.event.schema.json");
    check::<BrowserVisit>("browser.visit.jsonl", "browser.visit.schema.json");
    check::<AdRecord>("browser.ad.jsonl", "browser.ad.schema.json");
}

#[test]
fn board_spec_example_is_the_fixture_and_round_trips() {
    assert_doc_block("boards.md", "markdown", "board.md");
    let text = fixture("board.md");
    let board = trove_core::parse_board("sleep-calendar", &text).expect("board fixture parses");
    assert_eq!(board.title, "Sleep × Calendar");
    assert_eq!(board.panels.len(), 3);
    assert_eq!(board.panels[0].kind, trove_core::PanelKind::Tile);
    assert_eq!(board.panels[0].series[0].metric, "readiness-score");
    assert_eq!(board.panels[1].kind, trove_core::PanelKind::Dual);
    assert_eq!(board.panels[1].series[0].source, "oura");
    assert_eq!(board.panels[2].series[0].divide, Some(3600.0));
    assert!(board.panels.iter().flat_map(|p| &p.series).all(|s| s.is_valid()));
    assert_eq!(board.notes, "Does a packed calendar cost sleep?");
    // The app's writer produces exactly the documented text.
    assert_eq!(trove_core::render_board(&board).unwrap(), text);
}

#[test]
fn doc_examples_are_the_fixture_lines_verbatim() {
    let pairs = [
        ("domains/correspondence.md", "jsonl", "correspondence.message.jsonl"),
        ("domains/tasks.md", "jsonl", "tasks.task.jsonl"),
        ("domains/tasks.md", "jsonl-events", "tasks.event.jsonl"),
        ("domains/media-plays.md", "jsonl", "media.play.jsonl"),
        ("domains/calendar.md", "jsonl", "calendar.occurrence.jsonl"),
        ("domains/calendar.md", "jsonl-changes", "calendar.change.jsonl"),
        ("domains/contacts.md", "jsonl", "contacts.contact.jsonl"),
        ("domains/environment.md", "jsonl", "environment.reading.jsonl"),
        ("domains/environment.md", "jsonl-event", "environment.geo-event.jsonl"),
        ("domains/environment.md", "jsonl-almanac", "environment.almanac.jsonl"),
        ("domains/voice.md", "jsonl", "voice.recording.jsonl"),
        ("domains/notes.md", "jsonl", "notes.note.jsonl"),
        ("domains/social.md", "jsonl", "social.post.jsonl"),
        ("domains/meetings.md", "jsonl", "meetings.meeting.jsonl"),
        ("domains/photos.md", "jsonl", "photos.photo.jsonl"),
        ("domains/reading.md", "jsonl", "reading.item.jsonl"),
        ("domains/reading.md", "jsonl-highlight", "reading.highlight.jsonl"),
        ("domains/home.md", "jsonl", "home.reading.jsonl"),
        ("domains/habits.md", "jsonl", "habits.habit.jsonl"),
        ("domains/habits.md", "jsonl-checkin", "habits.checkin.jsonl"),
        ("domains/time-entries.md", "jsonl", "time-entries.entry.jsonl"),
        ("domains/finance-purchases.md", "jsonl", "finance-purchases.line-item.jsonl"),
        ("domains/health-nutrition.md", "jsonl", "health-nutrition.entry.jsonl"),
        ("domains/health-medical.md", "jsonl", "health-medical.observation.jsonl"),
        ("domains/health-sleep.md", "jsonl", "health-sleep.session.jsonl"),
        ("domains/travel.md", "jsonl", "travel.segment.jsonl"),
        ("domains/location.md", "jsonl", "location.fix.jsonl"),
        ("domains/browser-searches.md", "jsonl", "browser-searches.search.jsonl"),
        ("domains/activity.md", "jsonl", "activity.event.jsonl"),
        ("domains/browser-visits.md", "jsonl", "browser.visit.jsonl"),
        ("domains/ads.md", "jsonl", "browser.ad.jsonl"),
    ];
    for (page, tag, fixture_name) in pairs {
        assert_doc_block(page, tag, fixture_name);
    }
}

#[test]
fn phase3_draft_contracts_validate_against_schemas_and_docs() {
    // Every Phase 3 contract draft, as (spec page, fence tag, fixture, schema).
    // Each example validates against its schema and is its spec-page example
    // block verbatim. These are DRAFTS awaiting ratification — no Rust type, no
    // DOMAINS entry yet (see `check_schema_only`). A domain with several record
    // shapes (e.g. health-medical, home, environment) has one row per shape,
    // each with a unique fence tag within its page.
    let drafts = [
        // meetings.meeting is now RATIFIED (Rust type `Meeting` bound,
        // registered in DOMAINS, first collector `fathom`) — see the round-trip
        // check in `fixtures_validate_against_schemas_and_rust_types`.
        // voice.recording is now RATIFIED (Rust type `Recording` bound,
        // registered in DOMAINS, first collector apple-voice-memos) — see the
        // round-trip check in `fixtures_validate_against_schemas_and_rust_types`.
        // health-medical.observation is now RATIFIED (Rust type `Observation`
        // bound, registered in DOMAINS as one `health-medical` entry, first
        // collector `dexcom` — CGM estimated-glucose readings as observations) —
        // see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`. The sibling
        // health-medical.medication (jsonl-medication) and
        // health-medical.condition (jsonl-condition) stay drafts until a
        // collector binds each (the environment/nws precedent: one entry, bind
        // only what you write).
        ("domains/health-medical.md", "jsonl-medication", "health-medical.medication.jsonl", "health-medical.medication.schema.json"),
        ("domains/health-medical.md", "jsonl-condition", "health-medical.condition.jsonl", "health-medical.condition.schema.json"),
        // reading.item + reading.highlight are now RATIFIED (Rust types `Item`
        // and `Highlight` bound, registered in DOMAINS, first collector
        // `readwise` (Reader → items, Readwise → highlights)) — see the
        // round-trip checks in `fixtures_validate_against_schemas_and_rust_types`.
        // home.reading is now RATIFIED (Rust type `HomeReading` bound,
        // registered in DOMAINS as one `home` entry, first collector
        // `ambient-weather` — a personal weather station) — see the round-trip
        // check in `fixtures_validate_against_schemas_and_rust_types`. The
        // sibling home.event (device events) and home.energy (metered intervals)
        // stay drafts until a collector binds each (the environment/nws
        // precedent: one entry, bind only what you write).
        ("domains/home.md", "jsonl-event", "home.event.jsonl", "home.event.schema.json"),
        ("domains/home.md", "jsonl-energy", "home.energy.jsonl", "home.energy.schema.json"),
        // environment.reading + environment.geo-event + environment.almanac are
        // now RATIFIED (Rust types `EnvReading`/`EnvGeoEvent`/`Almanac` bound,
        // registered in DOMAINS as one `environment` entry, first almanac
        // collector `usno`) — see the round-trip checks in
        // `fixtures_validate_against_schemas_and_rust_types`.
        // location.fix is now RATIFIED (Rust type `Fix` bound, registered in
        // DOMAINS, first collector `google-timeline` (movement segments →
        // trails; place visits stay raw until a visits-shaped contract lands)) —
        // see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`.
        // travel.segment is now RATIFIED (Rust type `Segment` bound, registered
        // in DOMAINS, first collector `airbnb` — guest lodging stays as lodging
        // segments) — see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`.
        // habits.habit + habits.checkin are now RATIFIED (Rust types `Habit`
        // and `Checkin` bound, registered in DOMAINS, first collector
        // `habitica` (definitions → habits, task history → check-ins)) — see the
        // round-trip checks in `fixtures_validate_against_schemas_and_rust_types`.
        // time-entries.entry is now RATIFIED (Rust type `TimeEntry` bound,
        // registered in DOMAINS, first collector `toggl-track`) — see the
        // round-trip check in `fixtures_validate_against_schemas_and_rust_types`.
        // notes.note is now RATIFIED (Rust type `Note` bound, registered in
        // DOMAINS, first collector `bear`) — see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`.
        // photos.photo is now RATIFIED (Rust type `Photo` bound, registered in
        // DOMAINS, first collector `exif-import`) — see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`.
        // finance-purchases.line-item is now RATIFIED (Rust type `LineItem`
        // bound, registered in DOMAINS, first collector `bitcoin` — on-chain
        // value transfers as dated purchase line items) — see the round-trip
        // check in `fixtures_validate_against_schemas_and_rust_types`. The
        // sibling finance-holdings.position (a position snapshot, which a
        // wallet's tx history is not) stays a draft until a holdings source
        // binds it (the nws/environment precedent: one entry, bind only what
        // you write).
        ("domains/finance-holdings.md", "jsonl", "finance-holdings.position.jsonl", "finance-holdings.position.schema.json"),
        // social.post is now RATIFIED (Rust type `Post` bound, registered in
        // DOMAINS, first collector `facebook` (DYI import)) — see the
        // round-trip check in `fixtures_validate_against_schemas_and_rust_types`.
        // browser-searches.search is now RATIFIED (Rust type `Search` bound,
        // registered in DOMAINS, first collector `google-takeout` — the My
        // Activity Search export) — see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`. `safari` already
        // writes `browser/` visits and coexists here.
        // health-nutrition.entry is now RATIFIED (Rust type `NutritionEntry`
        // bound, registered in DOMAINS, first collector `cronometer` — the free
        // CSV food-log export) — see the round-trip check in
        // `fixtures_validate_against_schemas_and_rust_types`.
    ];
    for (page, tag, fixture_name, schema_name) in drafts {
        check_schema_only(fixture_name, schema_name);
        assert_doc_block(page, tag, fixture_name);
    }
}

#[test]
fn schema_required_lists_match_the_domain_registry() {
    let by_id = |id: &str| DOMAINS.iter().find(|c| c.id == id).unwrap();
    let schema_required = |name: &str| -> Vec<String> {
        let raw = fs::read_to_string(repo_root().join("docs/vault-spec/schemas").join(name)).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        v["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    };
    for (schema_name, domain) in [
        ("correspondence.message.schema.json", "correspondence"),
        ("tasks.task.schema.json", "tasks"),
        ("media.play.schema.json", "media-plays"),
        ("calendar.occurrence.schema.json", "calendar"),
        ("contacts.contact.schema.json", "contacts"),
        ("environment.reading.schema.json", "environment"),
        ("voice.recording.schema.json", "voice"),
        ("notes.note.schema.json", "notes"),
        ("social.post.schema.json", "social"),
        ("meetings.meeting.schema.json", "meetings"),
        ("photos.photo.schema.json", "photos"),
        ("reading.item.schema.json", "reading"),
        ("home.reading.schema.json", "home"),
        ("habits.habit.schema.json", "habits"),
        ("time-entries.entry.schema.json", "time-entries"),
        ("finance-purchases.line-item.schema.json", "finance-purchases"),
        ("health-nutrition.entry.schema.json", "health-nutrition"),
        ("health-medical.observation.schema.json", "health-medical"),
        ("health-sleep.session.schema.json", "health-sleep"),
        ("travel.segment.schema.json", "travel"),
        ("location.fix.schema.json", "location"),
        ("browser-searches.search.schema.json", "browser-searches"),
    ] {
        assert_eq!(
            schema_required(schema_name),
            by_id(domain).required.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "{schema_name} vs DOMAINS[{domain}].required"
        );
    }
}

#[test]
fn spec_pages_exist_for_every_named_domain() {
    for c in DOMAINS {
        let page = repo_root().join(c.spec_page);
        assert!(page.exists(), "{}: missing spec page {}", c.id, c.spec_page);
    }
}
