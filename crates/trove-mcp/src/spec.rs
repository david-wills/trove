//! The vault spec, embedded at build time so `describe_type` can hand an
//! agent the record shape for a domain without touching the filesystem (the
//! docs are not shipped inside `Trove.app`). One entry per page under
//! `docs/vault-spec/domains/`, plus the conventions page and a note for the
//! `health/` layout, which has no contract page yet.

/// One embedded spec page.
pub struct SpecPage {
    /// Domain id as used in `describe_type` — the page's file stem.
    pub id: &'static str,
    /// Repo path, for citation.
    pub path: &'static str,
    pub markdown: &'static str,
}

macro_rules! domain_page {
    ($id:literal) => {
        SpecPage {
            id: $id,
            path: concat!("docs/vault-spec/domains/", $id, ".md"),
            markdown: include_str!(concat!("../../../docs/vault-spec/domains/", $id, ".md")),
        }
    };
}

/// Every domain page, in the order of `docs/vault-spec/domains/`.
pub static DOMAIN_PAGES: &[SpecPage] = &[
    domain_page!("browser-searches"),
    domain_page!("calendar"),
    domain_page!("contacts"),
    domain_page!("correspondence"),
    domain_page!("environment"),
    domain_page!("finance-holdings"),
    domain_page!("finance-purchases"),
    domain_page!("habits"),
    domain_page!("health-medical"),
    domain_page!("health-nutrition"),
    domain_page!("home"),
    domain_page!("location"),
    domain_page!("media-plays"),
    domain_page!("meetings"),
    domain_page!("notes"),
    domain_page!("photos"),
    domain_page!("reading"),
    domain_page!("social"),
    domain_page!("tasks"),
    domain_page!("time-entries"),
    domain_page!("travel"),
    domain_page!("voice"),
];

/// The invariants every vault file holds (timestamps, partitions, `extra`,
/// `.trove/`), for `describe_type("conventions")`.
pub static CONVENTIONS: SpecPage = SpecPage {
    id: "conventions",
    path: "docs/vault-spec/conventions.md",
    markdown: include_str!("../../../docs/vault-spec/conventions.md"),
};

/// `health/` has no contract page yet (its spec is planned); this note is
/// what `describe_type("health")` returns so an agent knows the layout and
/// which tools read it.
pub static HEALTH_NOTE: SpecPage = SpecPage {
    id: "health",
    path: "docs/vault-spec/README.md",
    markdown: "\
# Domain: health (layout note — spec page planned)

Two layers live under `health/`:

- **Per-metric daily series** (Apple Health import): `health/<metric-slug>/daily.csv`
  with columns `date,count,sum,min,max,avg`, plus per-month raw sample CSVs
  `health/<metric-slug>/YYYY-MM.csv`. Slugs are kebab-case HealthKit types
  (`steps`, `heart-rate`, `sleep`, `hrv`, `resting-heart-rate`, …).
- **Raw per-collection JSONL** (Oura sync): `health/oura/<collection>.jsonl`
  holding untouched Oura API v2 records — `sleep` (one row per sleep
  session: `bedtime_start`, `bedtime_end`, `total_sleep_duration` in seconds,
  `average_hrv`, `average_heart_rate`, `efficiency`, `day`), `daily_sleep`,
  `daily_readiness`, `daily_activity`, `daily_stress`, `daily_spo2`,
  `enhanced_tag`, and `heartrate/YYYY-MM.jsonl` (5-minute samples).

Read them with the typed tools first: `health_metrics` lists every chartable
metric across sources (Apple Health and Oura merged under one canonical slug
with per-source ranges), and `health_series(metric, bucket)` returns one
day/week/month series per source. Use `read_stream(\"health/oura\")` only when
you need fields the canonical metrics do not carry. Those files are not
date-partitioned, but a day-precision `from`/`to` still filters records by
their `day` field, so ask for a small window and page with `next_offset`;
`sleep` rows are large (5-minute HR/HRV arrays), so pages end early.

Clinical and nutrition records are separate contract domains:
`describe_type(\"health-medical\")`, `describe_type(\"health-nutrition\")`.
",
};

/// Look a page up by id.
pub fn page(id: &str) -> Option<&'static SpecPage> {
    let id = id.trim().trim_end_matches(".md");
    if id == CONVENTIONS.id {
        return Some(&CONVENTIONS);
    }
    if id == HEALTH_NOTE.id {
        return Some(&HEALTH_NOTE);
    }
    DOMAIN_PAGES.iter().find(|p| p.id == id)
}

/// Every id `describe_type` accepts.
pub fn ids() -> Vec<&'static str> {
    let mut out: Vec<&str> = DOMAIN_PAGES.iter().map(|p| p.id).collect();
    out.push(HEALTH_NOTE.id);
    out.push(CONVENTIONS.id);
    out
}
