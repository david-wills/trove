//! Named domain contracts and the vault manifest.
//!
//! A **domain contract** is a normalized multi-source file format: any
//! collector — Rust module, script, or AI agent — plugs a new source into a
//! domain by writing its files under the domain's layout, and readers scan
//! the domain's folders with no registration (the `tasks/<source>/` and
//! `correspondence/<source>/` precedent, named and formalized). The public
//! field-level specs live in `docs/vault-spec/domains/`; this registry is
//! the in-code index that ties layout, partitioning, and spec page together
//! and is what the manifest builder and validation tests walk.
//!
//! The two-layer principle (lossless import): a source may keep *raw*
//! full-fidelity files in its own shape anywhere in its folder, and
//! *also/instead* writes the normalized contract shape with unmapped fields
//! preserved under `extra`. Opinions (unification, precedence, dedupe across
//! sources) happen only at read time, in the domain's reader module —
//! nothing derived is ever persisted.

use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store::Partition;
use crate::vault::Vault;

pub use crate::browser_searches::Search;
pub use crate::calendar::CalendarOccurrence;
pub use crate::contacts::Contact;
pub use crate::correspondence::Message;
pub use crate::environment::{Almanac, EnvGeoEvent, EnvReading};
pub use crate::finance::LineItem;
pub use crate::habits::{Checkin, Habit};
pub use crate::health_medical::Observation;
pub use crate::health_nutrition::Entry as NutritionEntry;
pub use crate::health_sleep::Session as SleepSession;
pub use crate::home::HomeReading;
pub use crate::media::MediaItem;
pub use crate::meetings::Meeting;
pub use crate::notes::Note;
pub use crate::photos::Photo;
pub use crate::reading::{Highlight, Item};
pub use crate::social::{Media, Post};
pub use crate::tasks::{Task, TaskEvent};
pub use crate::time_entries::TimeEntry;
pub use crate::travel::Segment;
pub use crate::voice::Recording;

/// The shape of a domain's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    /// Append-only date-partitioned JSONL (events happen, land once).
    EventStream,
    /// A rewritten current-state snapshot plus an append-only event stream.
    SnapshotPlusEvents,
    /// A rewritten current-state snapshot only — no event stream, no `ts`,
    /// no date partitions. Each `<source>/<account>.jsonl` is one whole file
    /// the source rewrites atomically (the `contacts` domain).
    Snapshot,
    /// Per-metric value series (`{ts|day, value, unit}` lines).
    MetricSeries,
}

impl ContractKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContractKind::EventStream => "event-stream",
            ContractKind::SnapshotPlusEvents => "snapshot-plus-events",
            ContractKind::Snapshot => "snapshot",
            ContractKind::MetricSeries => "metric-series",
        }
    }
}

/// One named multi-source contract.
pub struct DomainContract {
    /// Domain id, also the top of its layout path.
    pub id: &'static str,
    /// Human-readable layout, e.g. `correspondence/<source>/YYYY-MM.jsonl`.
    pub layout: &'static str,
    /// The vault-relative directory whose subdirectories are sources.
    pub root: &'static str,
    pub partition: Partition,
    pub kind: ContractKind,
    /// The spec page documenting the record fields.
    pub spec_page: &'static str,
    /// Fields every record must carry (asserted by the spec round-trip
    /// tests; readers stay lenient regardless).
    pub required: &'static [&'static str],
}

/// Every named contract. A domain earns an entry once at least two sources
/// (or one source plus a committed plan for more) write the same shape.
pub static DOMAINS: &[DomainContract] = &[
    DomainContract {
        id: "correspondence",
        layout: "correspondence/<source>/YYYY-MM.jsonl",
        root: "correspondence",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/correspondence.md",
        required: &["ts", "source", "chat", "sender", "kind"],
    },
    DomainContract {
        id: "tasks",
        layout: "tasks/<source>/tasks.jsonl + tasks/<source>/events/YYYY-MM.jsonl",
        root: "tasks",
        partition: Partition::Month,
        kind: ContractKind::SnapshotPlusEvents,
        spec_page: "docs/vault-spec/domains/tasks.md",
        required: &["source", "id", "title"],
    },
    DomainContract {
        id: "media-plays",
        layout: "media/plays/<source>/YYYY-MM.jsonl",
        root: "media/plays",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/media-plays.md",
        required: &["ts", "source", "category", "kind", "title", "subtitle", "seconds"],
    },
    DomainContract {
        id: "calendar",
        layout: "calendar/events/YYYY-MM.jsonl + calendar/changes/YYYY-MM.jsonl",
        root: "calendar",
        partition: Partition::Month,
        kind: ContractKind::SnapshotPlusEvents,
        spec_page: "docs/vault-spec/domains/calendar.md",
        required: &["id"],
    },
    DomainContract {
        id: "contacts",
        layout: "contacts/<source>/<account>.jsonl",
        root: "contacts",
        // `Snapshot` files are not date-partitioned, so `partition` is unused
        // here (kept non-optional for the shared struct); `scan_contract`
        // lists the per-account `*.jsonl` files directly instead of walking
        // date keys. Month is an inert placeholder.
        partition: Partition::Month,
        kind: ContractKind::Snapshot,
        spec_page: "docs/vault-spec/domains/contacts.md",
        required: &["source", "id"],
    },
    DomainContract {
        // One entry for the whole domain (like `tasks` covers Task+TaskEvent):
        // readings under `<source>/`, geo-events under `<source>/events/`,
        // almanacs under `<source>/almanac/` (almanac still a draft type). The
        // `required` list is the reading's — the primary scalar core — and is
        // kept in lockstep with environment.reading.schema.json. `scan_contract`
        // already walks the `["", "events", "changes"]` sub-paths per source,
        // so both `<source>/` (readings) and `<source>/events/` (geo-events)
        // are indexed with no scan change.
        id: "environment",
        layout: "environment/<source>/YYYY-MM.jsonl + environment/<source>/events/YYYY-MM.jsonl",
        root: "environment",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/environment.md",
        required: &["ts", "source", "metric", "value"],
    },
    DomainContract {
        // Spoken clips — voice memos and voicemails — in one append-only
        // stream, partitioned by the month of `ts`. A single record shape
        // ([`Recording`]); `kind` separates a memo from a voicemail. First
        // collector: `apple-voice-memos`.
        id: "voice",
        layout: "voice/<source>/YYYY-MM.jsonl",
        root: "voice",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/voice.md",
        required: &["ts", "source", "kind"],
    },
    DomainContract {
        // Collected notes from every notes/journaling/PKM app in one store. A
        // month-partitioned **snapshot** (per-affected-month whole-file atomic
        // rewrite), partitioned by the month of `created`, keyed by `id` within
        // a source ([`Note`]). Unlike `contacts` (account-named snapshot files),
        // notes files are date-named, so `scan_contract`'s Snapshot branch
        // harvests their dated stems for first/last. First collector: `bear`.
        id: "notes",
        layout: "notes/<source>/YYYY-MM.jsonl",
        root: "notes",
        partition: Partition::Month,
        kind: ContractKind::Snapshot,
        spec_page: "docs/vault-spec/domains/notes.md",
        required: &["source", "id"],
    },
    DomainContract {
        // Content the user authored on social platforms — posts, comments,
        // replies, reposts, quotes, edits — in one append-only stream,
        // partitioned by the month of `ts`. A single record shape ([`Post`]);
        // `kind` separates a top-level post from a reply/repost/edit. Likes,
        // saves, follows, and other non-authored sections stay per-source raw
        // under `social/<source>/raw/`. First collector: `facebook` (DYI
        // import).
        id: "social",
        layout: "social/<source>/YYYY-MM.jsonl",
        root: "social",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/social.md",
        required: &["ts", "source", "guid"],
    },
    DomainContract {
        // Every meeting an AI notetaker or platform captured — one record per
        // meeting (not per utterance), in one append-only stream partitioned by
        // the month of `ts`. A single record shape ([`Meeting`]); transcripts
        // ride as sidecar artifacts under `meetings/<source>/raw/`, pointed at
        // by `transcript_ref` (never inlined). A transcript that arrives on a
        // later poll upserts the same `guid` row in place. First collector:
        // `fathom`.
        id: "meetings",
        layout: "meetings/<source>/YYYY-MM.jsonl",
        root: "meetings",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/meetings.md",
        required: &["ts", "source", "guid"],
    },
    DomainContract {
        // Photo and video **metadata** — one record per asset (never image
        // bytes), in one append-only stream partitioned by the month of `ts`.
        // A single record shape ([`Photo`]); `kind` separates a photo from a
        // video/live/screenshot/burst. GPS geotags make this a privacy-
        // sensitive location proxy, so every source ships opt-in; faces/people
        // are a further opt-in (off by default, never resolved across sources).
        // Full source fidelity stays per-source raw under `photos/<source>/`.
        // First collector: `exif-import` (loose files dragged in).
        id: "photos",
        layout: "photos/<source>/YYYY-MM.jsonl",
        root: "photos",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/photos.md",
        required: &["ts", "source", "guid"],
    },
    DomainContract {
        // Everything the user saved, read, or marked up on the web and in books.
        // One entry for the whole domain (like `environment` covers
        // reading+geo-event): saved/read items under `<source>/`, annotations
        // under `<source>/highlights/`. Two record shapes ([`Item`] and
        // [`Highlight`]) share `ts·source·guid` as their required core, so the
        // `required` list — the items' core, kept in lockstep with
        // reading.item.schema.json — also fits every highlight. `scan_contract`
        // walks the `["", "highlights"]` sub-paths per source, so both
        // `<source>/` (items) and `<source>/highlights/` (highlights) are indexed
        // with no scan change. Feed lists, OPML, and article full-text stay
        // per-source raw under `<source>/`. First collector: `readwise` (Reader
        // → items, Readwise → highlights).
        id: "reading",
        layout: "reading/<source>/YYYY-MM.jsonl + reading/<source>/highlights/YYYY-MM.jsonl",
        root: "reading",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/reading.md",
        required: &["ts", "source", "guid"],
    },
    DomainContract {
        // Standalone GPS trails — every location fix, from any logger, in one
        // stream. One record shape ([`Fix`]): one line per fix, **day**-
        // partitioned (the only day-partitioned contract — a continuous logger
        // is high-volume, so month files would grow unwieldy). A track / logger
        // batch / movement segment / vehicle trip is the set of fixes sharing
        // one `trail` id (a read-time grouping), never a points array. `lat`/
        // `lon` are numbers (Google Timeline exports coordinate *strings* — the
        // collector parses them); SI units in-contract (m, m/s), a vehicle's
        // ambiguous odometer (km/mi) in `extra`. Privacy-sensitive (a continuous
        // where-you've-been trail): every source ships opt-in. Visits and saved
        // places (Swarm, Google Maps, Google Timeline / Arc place visits) are
        // place/visit-shaped, not fixes — they stay per-source raw under
        // `location/<source>/` until a visits-shaped contract lands. First
        // collector: `google-timeline` (movement segments → trails).
        id: "location",
        layout: "location/<source>/YYYY-MM-DD.jsonl",
        root: "location",
        partition: Partition::Day,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/location.md",
        required: &["ts", "source", "lat", "lon"],
    },
    DomainContract {
        // User-asserted time entries — what the owner *says* they spent time
        // on, logged by hand in a manual time tracker. One record shape
        // ([`TimeEntry`]) in one append-only stream, partitioned by the local
        // month of `start`. The deliberate counterpart to `activity/`
        // (observed auto-trackers write there, never here); the two merge only
        // at read time. Dedupe is on `id`, and the stream is append-only: the
        // first observed state of an id wins, so an entry first seen running is
        // logged open and its later stopped state lands only in raw/. First
        // collector: `toggl-track`.
        id: "time-entries",
        layout: "time-entries/<source>/YYYY-MM.jsonl",
        root: "time-entries",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/time-entries.md",
        required: &["source", "id", "start"],
    },
    DomainContract {
        // The routines the user tracks plus the per-day record of each. One
        // entry for the whole domain (like `tasks` covers Task+TaskEvent, and
        // `reading` covers Item+Highlight): the current habit definitions are a
        // rewritten **snapshot** at `<source>/habits.jsonl`, and the habit-day
        // check-ins are an append-only **event stream** under
        // `<source>/checkins/`. Two record shapes ([`Habit`] and [`Checkin`]);
        // the `required` list is the habit snapshot's — kept in lockstep with
        // habits.habit.schema.json. `scan_contract` walks the
        // `["", "events", "checkins", ...]` sub-paths per source, so both the
        // snapshot stem and the `<source>/checkins/` event files are indexed.
        // A habit's full raw payload (RPG progression, per-day history blob)
        // stays per-source raw under `<source>/raw/`. First collector:
        // `habitica` (habits + dailies → definitions, task history → check-ins).
        id: "habits",
        layout: "habits/<source>/habits.jsonl + habits/<source>/checkins/YYYY-MM.jsonl",
        root: "habits",
        partition: Partition::Month,
        kind: ContractKind::SnapshotPlusEvents,
        spec_page: "docs/vault-spec/domains/habits.md",
        required: &["source", "id", "title"],
    },
    DomainContract {
        // Itemized purchases — one record per purchased **line item** (an order
        // is a read-time grouping by `order_id`), in one append-only stream
        // partitioned by the month of `ts`. A single record shape
        // ([`LineItem`]). The enrichment layer beside the canonical `finance/`
        // ledger (an order and its charge stay separate records, joined at read
        // time); itemized spending is privacy-sensitive, so every source ships
        // opt-in. Full source fidelity stays per-source raw under
        // `finance/purchases/<source>/raw/`. First collector: `bitcoin`
        // (on-chain value transfers — the finance-purchases pioneer). The
        // sibling `finance-holdings` (position snapshots) stays a draft until a
        // holdings source binds it.
        id: "finance-purchases",
        layout: "finance/purchases/<source>/YYYY-MM.jsonl",
        root: "finance/purchases",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/finance-purchases.md",
        required: &["ts", "source", "guid", "merchant"],
    },
    DomainContract {
        // Everything an owned smart device or sensor records about the home. One
        // entry for the whole domain (like `environment` covers
        // reading+geo-event+almanac, and `reading` covers item+highlight):
        // scalar readings under `<source>/`, discrete device events under
        // `<source>/events/`, metered energy/water intervals under
        // `<source>/energy/`. Three record shapes; only [`HomeReading`] is bound
        // as a Rust type so far (first collector `ambient-weather`, a personal
        // weather station) — the event and energy shapes stay Phase-3 drafts
        // until a collector writes them (the `environment` precedent: one entry,
        // bind only what you write). The `required` list is the reading's — the
        // shared scalar core, kept in lockstep with home.reading.schema.json — so
        // it also fits every event/energy row once those bind. `scan_contract`
        // walks the `["", "events", "energy", …]` sub-paths per source, so the
        // reading stream and the (future) `events/` and `energy/` streams are all
        // indexed under one `home` entry with no scan change. The reading core is
        // identical to `environment`'s and the two merge at read time (ownership,
        // not shape, decides the folder).
        id: "home",
        layout: "home/<source>/YYYY-MM.jsonl + home/<source>/events/YYYY-MM.jsonl + home/<source>/energy/YYYY-MM.jsonl",
        root: "home",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/home.md",
        required: &["ts", "source", "metric", "value"],
    },
    DomainContract {
        // Trips broken into their segments — flights, lodging stays, car
        // rentals, trains, ferries — in one append-only stream partitioned by
        // the local month of `ts` (the segment's start: departure / check-in /
        // pickup). A single record shape ([`Segment`]); a `type` discriminator
        // separates a flight from a lodging stay / car / train / … Each source
        // writes its own `travel/<source>/` folder with its own stable `guid`
        // (the dedupe key), and two sources that captured the same flight both
        // write it — reconciliation is a read-time opinion, never a write-time
        // merge. Type-specific detail with no shared column (seat, room,
        // address, amount, …) rides under `extra`; full source fidelity also
        // stays per-source raw under `travel/<source>/raw/`. First collector:
        // `airbnb` (guest lodging stays → lodging segments).
        id: "travel",
        layout: "travel/<source>/YYYY-MM.jsonl",
        root: "travel",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/travel.md",
        required: &["ts", "source", "type", "guid"],
    },
    DomainContract {
        // What the user ate — one record per logged food entry, from every
        // nutrition tracker — in one append-only stream partitioned by the month
        // of `ts`. A single record shape ([`NutritionEntry`]): typed core macros
        // (`energy_kcal`/`protein_g`/`carb_g`/`fat_g`/…) plus a nested
        // `nutrients` map for the long micronutrient tail (Cronometer alone
        // exposes 80+ nutrients per food). `ts` admits a date-only `YYYY-MM-DD`
        // (Cronometer free tier / MacroFactor day rollups never timestamp the
        // diary day), which still partitions and sorts as a lexical prefix.
        // Cronometer, MyFitnessPal, MacroFactor, Lifesum, and the food-log
        // portion of Levels write this shape; a tracker's *other* exports route
        // by shape (weight/glucose → `health/`, exercise logs → per-source raw),
        // never here. Full source fidelity (the original CSVs) stays per-source
        // raw under `health/nutrition/<source>/raw/`. First collector:
        // `cronometer` (free CSV export).
        id: "health-nutrition",
        layout: "health/nutrition/<source>/YYYY-MM.jsonl",
        root: "health/nutrition",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/health-nutrition.md",
        required: &["ts", "source", "guid"],
    },
    DomainContract {
        // Sleep as sessions — one record per night, nap, or rest, as one
        // source observed it — the normalized convergence of every sleep
        // tracker (see [`SleepSession`]). Both shipping writers project raw
        // they already hold (Oura's `health/oura/sleep.jsonl`; an Apple Health
        // export's `SleepAnalysis` intervals, stitched per origin) and rewrite
        // their own month files whole, since both sources revise sessions
        // after the fact; a writer without raw appends with `guid` dedupe.
        // Apple Health is a relay, so the same night can arrive twice (Oura
        // directly and through Apple, marked by `origin`) — by design; the
        // read side hides the relay when the device writes its own folder
        // (`dedupe_relays`). The Apple importer's per-stage CSVs sit under the
        // same root as *files*; `scan_contract` reads directories only, so
        // they coexist. Partitioned by the month of `day` (the wake date).
        id: "health-sleep",
        layout: "health/sleep/<source>/YYYY-MM.jsonl",
        root: "health/sleep",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/health-sleep.md",
        required: &["day", "start", "end", "source", "guid"],
    },
    DomainContract {
        // Clinical records — lab results, vital signs, medications, and
        // conditions — normalized out of FHIR (and non-FHIR fallbacks). One
        // entry for the whole domain (like `reading` covers item+highlight and
        // `environment` covers reading+geo-event+almanac): observations under
        // `<source>/observations/`, medications under `<source>/medications/`,
        // conditions under `<source>/conditions/`. Only [`Observation`] is bound
        // as a Rust type so far (first collector `dexcom`, whose CGM
        // estimated-glucose readings are observations); the sibling medication
        // and condition shapes stay Phase-3 drafts until a collector writes them
        // (the environment/nws precedent: one entry, bind only what you write).
        // The `required` list is the observation's core — kept in lockstep with
        // health-medical.observation.schema.json. `scan_contract` walks the
        // `["", "observations", "medications", "conditions", …]` sub-paths per
        // source, so all three streams are indexed under one entry with no scan
        // change. Privacy-sensitive (medical): every source ships opt-in with
        // explicit acknowledgement, and full-fidelity raw resources stay
        // per-source under `<source>/raw/`. First collector: `dexcom`.
        id: "health-medical",
        layout: "health/medical/<source>/observations/YYYY-MM.jsonl + health/medical/<source>/medications/YYYY-MM.jsonl + health/medical/<source>/conditions/YYYY-MM.jsonl",
        root: "health/medical",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/health-medical.md",
        required: &["ts", "source", "guid", "test"],
    },
    DomainContract {
        // Every search query the owner typed, from any search source, in one
        // append-only stream partitioned by the month of `ts`. A single record
        // shape ([`Search`]) — one query at one time; only `ts·source·query` is
        // required (a sparse source needs no more), while `engine`/`url`/`guid`
        // are optional enrichment. This is the query stream only: the page
        // visited *after* a search is a `browser/` visit (a sibling stream),
        // joined at read time, so there is no `result_clicked` field. Each
        // source keeps its own `browser/searches/<source>/` folder and stable
        // guids; the same query seen twice is reconciled at read time, never
        // merged at write time. Source-specific detail rides under `extra`; full
        // source fidelity stays per-source raw under
        // `browser/searches/<source>/raw/`. First collector: `google-takeout`
        // (My Activity Search export). `safari` already writes `browser/` visits
        // and coexists here.
        id: "browser-searches",
        layout: "browser/searches/<source>/YYYY-MM.jsonl",
        root: "browser/searches",
        partition: Partition::Month,
        kind: ContractKind::EventStream,
        spec_page: "docs/vault-spec/domains/browser-searches.md",
        required: &["ts", "source", "query"],
    },
];

// ---------------------------------------------------------------------------
// The manifest: `.trove/manifest.json`

const MANIFEST_FILE: &str = ".trove/manifest.json";

/// A rebuildable index of what's in the vault, for analyzers, LLMs, and the
/// generic data browser to orient cheaply. Never authoritative — readers
/// scan files, the manifest only points.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Manifest {
    /// RFC3339 local time of the last rebuild.
    pub updated: String,
    pub domains: Vec<ManifestDomain>,
}

/// One data folder: a named contract, or a best-effort entry for any folder
/// of date-named JSONL a collector invented (M6 sources appear without
/// registration).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ManifestDomain {
    /// The contract id, or the folder path for unregistered streams.
    pub domain: String,
    pub layout: String,
    /// "event-stream" | "snapshot-plus-events" | "snapshot" | "metric-series".
    pub kind: String,
    /// Source folders present (empty for single-stream folders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Oldest / newest partition key present (YYYY-MM or YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Partition files present (cheap; line counts are not maintained).
    pub files: u64,
    /// Spec page for named contracts, "" for discovered folders.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spec: String,
}

impl Vault {
    /// The manifest as last built (missing = empty).
    pub fn read_manifest(&self) -> Manifest {
        let Ok(path) = self.resolve(MANIFEST_FILE) else {
            return Manifest::default();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Walk the vault and rebuild `.trove/manifest.json`: one entry per
    /// named contract with data, plus best-effort entries for any other
    /// folder holding date-named JSONL (so novel collector folders appear).
    /// Cheap — directory listings only, no file contents.
    pub fn rebuild_manifest(&self) -> Result<Manifest> {
        let mut domains = Vec::new();
        let mut covered: Vec<&str> = Vec::new();
        for c in DOMAINS {
            covered.push(c.root);
            if let Some(entry) = self.scan_contract(c) {
                domains.push(entry);
            }
        }
        // Best-effort discovery of everything else: top-level data folders
        // (and one subdirectory level) holding date-named JSONL.
        let root = self.root();
        let mut tops: Vec<String> = fs::read_dir(root)
            .map(|es| {
                es.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.') && n != "artifacts" && n != "inbox")
                    .collect()
            })
            .unwrap_or_default();
        tops.sort();
        for top in tops {
            for dir in [top.clone()].into_iter().chain(subdirs(self, &top)) {
                if covered.iter().any(|c| dir == *c || dir.starts_with(&format!("{c}/"))) {
                    continue;
                }
                if let Some(entry) = self.scan_discovered(&dir) {
                    domains.push(entry);
                }
            }
        }
        let manifest = Manifest {
            updated: chrono::Local::now().to_rfc3339(),
            domains,
        };
        crate::store::write_atomic(
            &self.resolve(MANIFEST_FILE)?,
            serde_json::to_string_pretty(&manifest)?.as_bytes(),
        )?;
        Ok(manifest)
    }

    /// Manifest entry for a named contract, `None` when it holds no data.
    fn scan_contract(&self, c: &DomainContract) -> Option<ManifestDomain> {
        let root = self.root().join(c.root);
        let mut sources: Vec<String> = fs::read_dir(&root)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        sources.sort();
        let mut keys: Vec<String> = Vec::new();
        let mut files = 0u64;
        for source in &sources {
            if c.kind == ContractKind::Snapshot {
                // A snapshot domain's files are one whole file each, named by
                // whatever partitions the source. `contacts` names them by
                // account (`<source>/<account>.jsonl`); `notes` names them by
                // month of `created` (`<source>/YYYY-MM.jsonl`). Count them all,
                // and harvest only the *date-looking* stems as first/last keys —
                // so account stems contribute none (contacts: no keys, as
                // before) while date stems give a notes source its real span.
                let dir = format!("{}/{source}", c.root);
                if let Ok(parts) = self.stream(&dir, c.partition).partitions() {
                    files += parts.len() as u64;
                    keys.extend(parts.into_iter().filter(|p| looks_dated(p)));
                }
                continue;
            }
            // Snapshot+events domains keep their stream under events/ (or
            // changes/, or habits' checkins/); the reading domain keeps
            // annotations under highlights/; the environment domain keeps
            // almanacs under almanac/; the home domain keeps energy intervals
            // under energy/; plain streams partition directly under the source.
            // A sub-path a given domain doesn't use simply finds no files. (The
            // habits snapshot `<source>/habits.jsonl` is matched by the ""
            // sub-path; its non-dated `habits` stem is file-counted but
            // contributes no first/last key.)
            for sub in [
                "", "events", "changes", "highlights", "checkins", "almanac", "energy",
                "observations", "medications", "conditions",
            ] {
                let dir = if sub.is_empty() {
                    format!("{}/{source}", c.root)
                } else {
                    format!("{}/{source}/{sub}", c.root)
                };
                if let Ok(parts) = self.stream(&dir, c.partition).partitions() {
                    files += parts.len() as u64;
                    keys.extend(parts);
                }
            }
        }
        if files == 0 && sources.is_empty() {
            return None;
        }
        keys.sort();
        Some(ManifestDomain {
            domain: c.id.to_string(),
            layout: c.layout.to_string(),
            kind: c.kind.as_str().to_string(),
            sources,
            first: keys.first().cloned(),
            last: keys.last().cloned(),
            files,
            spec: c.spec_page.to_string(),
        })
    }

    /// Best-effort entry for an unregistered folder of date-named JSONL.
    fn scan_discovered(&self, dir: &str) -> Option<ManifestDomain> {
        let parts = self.stream(dir, Partition::Day).partitions().ok()?;
        let dated: Vec<String> = parts
            .into_iter()
            .filter(|p| looks_dated(p))
            .collect();
        if dated.is_empty() {
            return None;
        }
        let day_named = dated.iter().any(|p| p.len() == 10);
        Some(ManifestDomain {
            domain: dir.to_string(),
            layout: format!("{dir}/{}.jsonl", if day_named { "YYYY-MM-DD" } else { "YYYY-MM" }),
            kind: ContractKind::EventStream.as_str().to_string(),
            sources: Vec::new(),
            first: dated.first().cloned(),
            last: dated.last().cloned(),
            files: dated.len() as u64,
            spec: String::new(),
        })
    }
}

/// One level of subdirectories of a top-level vault folder, as
/// vault-relative paths.
fn subdirs(vault: &Vault, top: &str) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(vault.root().join(top))
        .map(|es| {
            es.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| format!("{top}/{}", e.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Does a file stem look like a date partition (YYYY-MM or YYYY-MM-DD)?
fn looks_dated(stem: &str) -> bool {
    let b = stem.as_bytes();
    let date_ish = |b: &[u8]| {
        b.len() >= 7
            && b[..4].iter().all(|c| c.is_ascii_digit())
            && b[4] == b'-'
            && b[5..7].iter().all(|c| c.is_ascii_digit())
    };
    match b.len() {
        7 => date_ish(b),
        10 => date_ish(b) && b[7] == b'-' && b[8..10].iter().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-contracts-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write(v: &Vault, rel: &str, body: &str) {
        let path = v.root().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn domain_ids_and_specs_are_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for c in DOMAINS {
            assert!(seen.insert(c.id), "duplicate domain {}", c.id);
            assert!(c.spec_page.starts_with("docs/vault-spec/domains/"), "{}", c.id);
            assert!(!c.required.is_empty(), "{}", c.id);
        }
    }

    #[test]
    fn snapshot_branch_harvests_dated_stems_but_not_account_stems() {
        // The `ContractKind::Snapshot` scan refinement: notes files are
        // date-named (`<source>/YYYY-MM.jsonl`) → their stems become first/last
        // keys; contacts files are account-named (`<source>/<account>.jsonl`) →
        // their stems are NOT date-shaped, so they contribute no keys. Both are
        // file-counted. This guards the refinement that lets `notes` (and any
        // future date-named snapshot) report a span while `contacts` stays as it
        // always was.
        let v = temp_vault("snapshot-branch");
        // notes: date-named snapshot stems across two months.
        write(&v, "notes/bear/2026-03.jsonl", "{\"source\":\"bear\",\"id\":\"a\"}\n");
        write(&v, "notes/bear/2026-05.jsonl", "{\"source\":\"bear\",\"id\":\"b\"}\n");
        // contacts: account-named snapshot stem (not a date).
        write(&v, "contacts/google-contacts/default.jsonl", "{\"source\":\"google-contacts\",\"id\":\"c1\"}\n");

        let m = v.rebuild_manifest().unwrap();

        let notes = m.domains.iter().find(|d| d.domain == "notes").unwrap();
        assert_eq!(notes.sources, vec!["bear"]);
        assert_eq!(notes.first.as_deref(), Some("2026-03"), "dated stems → first key");
        assert_eq!(notes.last.as_deref(), Some("2026-05"), "dated stems → last key");
        assert_eq!(notes.files, 2);

        let contacts = m.domains.iter().find(|d| d.domain == "contacts").unwrap();
        assert_eq!(contacts.sources, vec!["google-contacts"]);
        assert_eq!(contacts.first, None, "account stem is not dated → no first key");
        assert_eq!(contacts.last, None, "account stem is not dated → no last key");
        assert_eq!(contacts.files, 1, "still file-counted");
    }

    #[test]
    fn manifest_indexes_contracts_and_discovers_novel_folders() {
        let v = temp_vault("manifest");
        // A contract domain with two sources.
        write(&v, "correspondence/imessage/2026-05.jsonl", "{}\n");
        write(&v, "correspondence/email/2026-06.jsonl", "{}\n");
        // A top-level stream (activity-shaped).
        write(&v, "activity/2026-06-11.jsonl", "{}\n");
        // A novel M6 collector folder nobody registered.
        write(&v, "dreams/journal/2026-06.jsonl", "{}\n");

        let m = v.rebuild_manifest().unwrap();
        let corr = m.domains.iter().find(|d| d.domain == "correspondence").unwrap();
        assert_eq!(corr.sources, vec!["email", "imessage"]);
        assert_eq!(corr.first.as_deref(), Some("2026-05"));
        assert_eq!(corr.last.as_deref(), Some("2026-06"));
        assert_eq!(corr.files, 2);
        assert!(!corr.spec.is_empty());

        let act = m.domains.iter().find(|d| d.domain == "activity").unwrap();
        assert_eq!(act.layout, "activity/YYYY-MM-DD.jsonl");
        let dreams = m.domains.iter().find(|d| d.domain == "dreams/journal").unwrap();
        assert_eq!(dreams.last.as_deref(), Some("2026-06"));

        // Rebuild is idempotent (modulo the timestamp) and readable back.
        let m2 = v.rebuild_manifest().unwrap();
        assert_eq!(m.domains, m2.domains);
        assert_eq!(v.read_manifest().domains, m.domains);
    }

    #[test]
    fn media_plays_contract_records_validate_required_fields() {
        // The envelope every media-plays line must carry — kept in lockstep
        // with the registry's `required` list and the spec page.
        let c = DOMAINS.iter().find(|c| c.id == "media-plays").unwrap();
        let line = json!({
            "ts": "2026-06-10T21:00:00-07:00",
            "source": "letterboxd",
            "category": "video",
            "kind": "play",
            "title": "Heat",
            "subtitle": "Michael Mann",
            "seconds": 0,
            "extra": {"rating": "5"}
        });
        for f in c.required {
            assert!(line.get(*f).is_some(), "example must carry {f}");
        }
        let item: MediaItem = serde_json::from_value(line).unwrap();
        assert_eq!(item.source, "letterboxd");
        assert_eq!(item.extra.get("rating"), Some(&serde_json::Value::String("5".into())));
    }

    #[test]
    fn reading_contract_covers_items_and_highlights() {
        // One DOMAINS entry covers both reading shapes; the required core
        // (`ts·source·guid`) fits an Item and a Highlight alike, and a
        // highlights/ sub-path is indexed alongside the items.
        let c = DOMAINS.iter().find(|c| c.id == "reading").unwrap();
        assert_eq!(c.required, &["ts", "source", "guid"]);

        let item_line = json!({
            "ts": "2026-06-10T14:03:00-07:00",
            "source": "raindrop",
            "guid": "rd-1029384",
            "progress": 63,
            "extra": {"collection": "Reading"}
        });
        let hl_line = json!({
            "ts": "2026-06-08T20:11:00-07:00",
            "source": "readwise",
            "guid": "rw-hl-884412",
            "text": "Attention is the rarest and purest form of generosity."
        });
        for f in c.required {
            assert!(item_line.get(*f).is_some(), "item example must carry {f}");
            assert!(hl_line.get(*f).is_some(), "highlight example must carry {f}");
        }
        let it: Item = serde_json::from_value(item_line).unwrap();
        assert_eq!(it.progress, Some(63), "progress is an integer percent");
        let hl: Highlight = serde_json::from_value(hl_line).unwrap();
        assert_eq!(hl.source, "readwise");

        // The manifest indexes both the item stream and the highlights/ stream
        // under one `reading` domain entry (the highlights/ sub-path scan).
        let v = temp_vault("reading-manifest");
        write(&v, "reading/readwise/2026-06.jsonl", "{\"ts\":\"2026-06-10T14:03:00-07:00\",\"source\":\"readwise\",\"guid\":\"doc-1\"}\n");
        write(&v, "reading/readwise/highlights/2026-06.jsonl", "{\"ts\":\"2026-06-08T20:11:00-07:00\",\"source\":\"readwise\",\"guid\":\"rw-hl-884412\"}\n");
        let m = v.rebuild_manifest().unwrap();
        let reading = m.domains.iter().find(|d| d.domain == "reading").unwrap();
        assert_eq!(reading.sources, vec!["readwise"]);
        assert_eq!(reading.files, 2, "both the item file and the highlights file counted");
        assert!(!reading.spec.is_empty());
    }
}
