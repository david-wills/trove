//! AwardWallet — loyalty-program balance and travel-timeline sync via the
//! AwardWallet Account Access API.
//!
//! AwardWallet aggregates 700+ loyalty programs (airlines, hotels, credit
//! cards, etc.) and exposes them through a commercial REST API authenticated
//! with an API key in the `X-Authentication` header. A **paid Business**
//! account is required for the travel-timeline endpoints; the account-list
//! endpoint (balances only) is available to registered developers.
//!
//! ## What this module pulls
//!
//! - **Loyalty account balances** (`/api/export/v1/account/{id}`) — program
//!   name, balance, member id, status, expiry, and transaction history.
//!   Snapshot-style: balance at the time of each sync. Routed to the raw
//!   layer; the balance-snapshot shape fits the unbound `finance-holdings`
//!   draft, not the bound `finance-purchases` contract (which is for dated
//!   transfers, not standing balances). See `contract_mode` note below.
//! - **Travel reservations** (`/api/export/v1/travel-timeline/{id}`) — flights,
//!   hotels, and car rentals as itinerary objects. Each segment maps to a
//!   [`crate::travel::Segment`] on the bound travel contract. Requires a paid
//!   Business subscription; degrades gracefully (skips with a hint) on a free
//!   account.
//!
//! ## Auth
//!
//! The API uses header-based API key authentication, NOT OAuth:
//! `X-Authentication: <api_key>`. The brief described this as "OAuth"; the
//! actual published documentation specifies API key only. The key is obtained
//! from `business.awardwallet.com/profile/api` after registering a Business
//! account. Stored under `.trove/sync/awardwallet.json` (0600) via the
//! standard TokenPaste mechanism.
//!
//! ## Raw-layer paths
//!
//! - `travel/awardwallet/raw/YYYY.jsonl`    — reservation objects verbatim
//! - `travel/awardwallet/raw/balances/YYYY-MM.jsonl` — account balance objects
//!
//! ## Contract layer
//!
//! - `travel/awardwallet/YYYY-MM.jsonl` — [`crate::travel::Segment`] rows
//!   (flight / lodging / car) partitioned by segment-start month.
//! - Balance snapshots: raw-only (no bound contract; `finance-holdings` draft
//!   is the eventual home but is unbound as of Phase-3).
//!
//! ## Parser status
//!
//! The API shape is fully documented on the official Swagger UI; field names
//! are confirmed from the live documentation. A paid Business account is
//! required to exercise the travel-timeline endpoint in practice, so the
//! travel-segment parser is scaffolded with documented field names and parked
//! behind `Needs-login`. The balance poller can run on a free developer
//! account once Trove registers an app.
//!
//! ## Deduplication
//!
//! - Reservations: composite `guid` = `"{type}-{reservation_date}-{confirmation}"`.
//!   When no confirmation number is present, a hash of type + departure + arrival
//!   date is used. Cursor in `.trove/awardwallet-sync.json` (rebuildable).
//! - Balances: keyed on `(accountId, snapshot_month)` — one raw row per
//!   account per month, overwritten on re-pull.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::{write_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::travel::Segment;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Paths / constants

/// Contract-layer travel segments.
const DIR: &str = "travel/awardwallet";
/// Raw reservation objects (one file per reservation year).
const RAW_DIR: &str = "travel/awardwallet/raw";
/// Raw balance snapshot objects (one file per snapshot month).
const RAW_BAL_DIR: &str = "travel/awardwallet/raw/balances";
/// Non-secret cursor (watermark).
const SYNC_FILE: &str = ".trove/awardwallet-sync.json";
/// Secret-store service id.
const SERVICE: &str = "awardwallet";

/// AwardWallet Account Access API base URL (documented at awardwallet.com/api/account).
const API_BASE: &str = "https://business.awardwallet.com";

/// HTTP connect + read timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 6-hour poll cadence: balances don't change often, and a Business API key
/// is rate-limited. Reservations are checked on the same cadence.
pub const AWARDWALLET_SYNC_SECS: u64 = 21_600;

// ---------------------------------------------------------------------------
// Cursor

/// Persisted watermark — stores the last-known member id (from the API) and the
/// set of reservation guids already written.  Non-secret so it lives beside the
/// other `.trove/` indexes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// Last-known user id from the `/connectedUser` endpoint.
    #[serde(default)]
    user_id: Option<i64>,
    /// ISO date of the most recent balance snapshot (YYYY-MM-DD).
    #[serde(default)]
    last_balance_date: Option<String>,
    /// Most recent travel-timeline start date fetched (YYYY-MM-DD).
    #[serde(default)]
    timeline_since: Option<String>,
}

fn load_cursor(vault: &Vault) -> Cursor {
    let path = vault.root().join(SYNC_FILE);
    let Ok(bytes) = std::fs::read(&path) else { return Cursor::default() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_cursor(vault: &Vault, cursor: &Cursor) -> Result<()> {
    let path = vault.root().join(SYNC_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, serde_json::to_vec_pretty(cursor)?.as_slice())
}

// ---------------------------------------------------------------------------
// API shapes — exact field names from the official Swagger documentation at
// https://awardwallet.com/api/account (confirmed 2026-06-17).

/// A single confirmation-number object as returned in the `confirmationNumbers`
/// array of a travel itinerary.  Real API shape (from official Swagger docs):
/// `{"number": "J3HND-8776", "description": "...", "isPrimary": true}`.
/// All fields are optional to be defensive against partial objects.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AwConfNumber {
    number: Option<String>,
    #[serde(default)]
    is_primary: bool,
    #[allow(dead_code)]
    description: Option<String>,
}

/// A travel itinerary item as returned by
/// `POST /api/export/v1/travel-timeline/{id}`.
/// `type` discriminates: `flight` | `carRental` | `hotelReservation` | `bus`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AwItinerary {
    #[serde(rename = "type")]
    type_: Option<String>,
    status: Option<String>,
    reservation_date: Option<String>,
    cancelled: Option<bool>,
    /// Each element is `{"number": "...", "isPrimary": true, ...}`.
    /// Pick the primary one (or first) for the contract `confirmation` field.
    #[serde(default)]
    confirmation_numbers: Vec<AwConfNumber>,
    // Flight-specific
    #[serde(default)]
    segments: Vec<FlightSegment>,
    // Hotel-specific
    hotel_name: Option<String>,
    chain_name: Option<String>,
    address: Option<Value>,
    check_in_date: Option<String>,
    check_out_date: Option<String>,
    // Car-specific
    pickup: Option<Value>,
    dropoff: Option<Value>,
    rental_company: Option<String>,
    // Pricing info
    pricing_info: Option<Value>,
}

/// A single flight segment within an [`AwItinerary`].
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FlightSegment {
    departure: Option<TripLocation>,
    arrival: Option<TripLocation>,
    marketing_carrier: Option<MarketingCarrier>,
    cabin: Option<String>,
    duration: Option<String>,
    // Kept for completeness; cancellation is checked at the itinerary level.
    #[allow(dead_code)]
    cancelled: Option<bool>,
}

/// A flight departure or arrival location from the API.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct TripLocation {
    airport_code: Option<String>,
    name: Option<String>,
    terminal: Option<String>,
    local_date_time: Option<String>,
}

/// The marketing carrier for a flight segment.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MarketingCarrier {
    airline: Option<Value>,   // contains { name, iataCode, … }
    flight_number: Option<String>,
    confirmation_number: Option<String>,
}

// ---------------------------------------------------------------------------
// Mapping helpers

/// Map one [`AwItinerary`] to one or more travel-contract [`Segment`]s.
/// A flight itinerary may contain multiple [`FlightSegment`]s (legs); each
/// becomes its own `Segment`. Hotel and car reservations map to one segment.
/// Returns an empty vec when the itinerary has no mappable data.
fn itinerary_to_segments(itin: &AwItinerary, _raw: &Map<String, Value>) -> Vec<Segment> {
    let type_ = itin.type_.as_deref().unwrap_or("");
    let status = if itin.cancelled.unwrap_or(false) {
        "cancelled".to_string()
    } else {
        itin.status.clone().unwrap_or_default()
    };
    // Pick the primary confirmation number, or the first one, or empty string.
    let confirmation = itin
        .confirmation_numbers
        .iter()
        .find(|c| c.is_primary)
        .or_else(|| itin.confirmation_numbers.first())
        .and_then(|c| c.number.as_deref())
        .unwrap_or("")
        .to_string();

    match type_ {
        "flight" => {
            let mut out = Vec::new();
            for (i, seg) in itin.segments.iter().enumerate() {
                let dep = seg.departure.as_ref();
                let arr = seg.arrival.as_ref();
                let ts = dep
                    .and_then(|d| d.local_date_time.as_deref())
                    .and_then(parse_aw_datetime);
                let Some(ts) = ts else { continue };

                let origin_code = dep
                    .and_then(|d| d.airport_code.as_deref())
                    .unwrap_or("")
                    .to_string();
                let origin_name = dep.and_then(|d| d.name.as_deref()).unwrap_or("").to_string();
                let dest_code = arr
                    .and_then(|d| d.airport_code.as_deref())
                    .unwrap_or("")
                    .to_string();
                let dest_name = arr.and_then(|d| d.name.as_deref()).unwrap_or("").to_string();

                let mc = seg.marketing_carrier.as_ref();
                let flight_number = mc.and_then(|c| c.flight_number.as_deref()).unwrap_or("");
                let airline_name = mc
                    .and_then(|c| c.airline.as_ref())
                    .and_then(|a| a.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let seg_conf =
                    mc.and_then(|c| c.confirmation_number.as_deref()).unwrap_or(&confirmation);

                // guid: prefer per-segment confirmation; else hash route+ts
                let guid = if !seg_conf.is_empty() && !origin_code.is_empty() {
                    format!("aw-flight-{seg_conf}-{i}")
                } else {
                    format!(
                        "aw-flight-{}-{}-{}",
                        origin_code,
                        dest_code,
                        ts.get(..10).unwrap_or(&ts)
                    )
                };

                let mut s = Segment::new("awardwallet", "flight", guid, &ts);
                if let Some(end_ts) = arr
                    .and_then(|d| d.local_date_time.as_deref())
                    .and_then(parse_aw_datetime)
                {
                    s.end_ts = end_ts;
                }
                s.start_place = origin_code;
                s.start_place_name = origin_name;
                s.end_place = dest_code;
                s.end_place_name = dest_name;
                s.vendor = airline_name.to_string();
                s.number = flight_number.to_string();
                s.confirmation = seg_conf.to_string();
                s.booking_id = confirmation.clone();
                s.status = status.clone();

                let mut extra = Map::new();
                let mut put = |k: &str, v: &str| {
                    if !v.trim().is_empty() {
                        extra.insert(k.into(), Value::String(v.to_string()));
                    }
                };
                put("cabin", seg.cabin.as_deref().unwrap_or(""));
                put("duration", seg.duration.as_deref().unwrap_or(""));
                if let Some(dep_term) = dep.and_then(|d| d.terminal.as_deref()) {
                    put("departure_terminal", dep_term);
                }
                if let Some(arr_term) = arr.and_then(|a| a.terminal.as_deref()) {
                    put("arrival_terminal", arr_term);
                }
                if !extra.is_empty() {
                    s.extra = extra;
                }
                out.push(s);
            }
            out
        }

        "hotelReservation" => {
            let ts = itin
                .check_in_date
                .as_deref()
                .and_then(|d| {
                    NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(|nd| {
                        use chrono::TimeZone;
                        use chrono::NaiveTime;
                        Local
                            .from_local_datetime(&nd.and_time(
                                NaiveTime::from_hms_opt(15, 0, 0).expect("valid time"),
                            ))
                            .earliest()
                            .map(|dt| dt.to_rfc3339())
                    })
                })
                .flatten()
                .or_else(|| {
                    itin.reservation_date
                        .as_deref()
                        .and_then(parse_aw_datetime)
                });

            let Some(ts) = ts else { return vec![] };
            let hotel = itin.hotel_name.as_deref().unwrap_or("");
            let chain = itin.chain_name.as_deref().unwrap_or("");
            let city = itin
                .address
                .as_ref()
                .and_then(|a| a.get("city"))
                .and_then(|c| c.as_str())
                .unwrap_or("");

            let guid = if !confirmation.is_empty() {
                format!("aw-hotel-{confirmation}")
            } else {
                format!("aw-hotel-{hotel}-{}", ts.get(..10).unwrap_or(&ts))
            };

            let mut s = Segment::new("awardwallet", "lodging", guid, &ts);
            if let Some(end_ts) = itin.check_out_date.as_deref().and_then(|d| {
                NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(|nd| {
                    use chrono::TimeZone;
                    use chrono::NaiveTime;
                    Local
                        .from_local_datetime(&nd.and_time(
                            NaiveTime::from_hms_opt(11, 0, 0).expect("valid time"),
                        ))
                        .earliest()
                        .map(|dt| dt.to_rfc3339())
                })
            }) {
                if let Some(end) = end_ts {
                    s.end_ts = end;
                }
            }
            s.start_place = city.to_string();
            s.start_place_name = hotel.to_string();
            s.vendor = chain.to_string();
            s.confirmation = confirmation.clone();
            s.status = status;

            let mut extra = Map::new();
            if let Some(addr) = &itin.address {
                extra.insert("address".into(), addr.clone());
            }
            if let Some(pi) = &itin.pricing_info {
                extra.insert("pricing_info".into(), pi.clone());
            }
            if !extra.is_empty() {
                s.extra = extra;
            }
            vec![s]
        }

        "carRental" => {
            let pickup_dt = itin
                .pickup
                .as_ref()
                .and_then(|p| p.get("localDateTime"))
                .and_then(|d| d.as_str())
                .and_then(parse_aw_datetime)
                .or_else(|| itin.reservation_date.as_deref().and_then(parse_aw_datetime));

            let Some(ts) = pickup_dt else { return vec![] };
            let company = itin.rental_company.as_deref().unwrap_or("");
            let pickup_location = itin
                .pickup
                .as_ref()
                .and_then(|p| p.get("locationName"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let dropoff_location = itin
                .dropoff
                .as_ref()
                .and_then(|d| d.get("locationName"))
                .and_then(|n| n.as_str())
                .unwrap_or("");

            let guid = if !confirmation.is_empty() {
                format!("aw-car-{confirmation}")
            } else {
                format!("aw-car-{company}-{}", ts.get(..10).unwrap_or(&ts))
            };

            let mut s = Segment::new("awardwallet", "car", guid, &ts);
            if let Some(drop_dt) = itin
                .dropoff
                .as_ref()
                .and_then(|d| d.get("localDateTime"))
                .and_then(|d| d.as_str())
                .and_then(parse_aw_datetime)
            {
                s.end_ts = drop_dt;
            }
            s.start_place_name = pickup_location.to_string();
            s.end_place_name = dropoff_location.to_string();
            s.vendor = company.to_string();
            s.confirmation = confirmation;
            s.status = status;

            if let Some(pi) = &itin.pricing_info {
                let mut extra = Map::new();
                extra.insert("pricing_info".into(), pi.clone());
                s.extra = extra;
            }
            vec![s]
        }

        _ => vec![],
    }
}

/// Parse an AwardWallet datetime string (ISO 8601 / RFC3339 or a bare
/// `YYYY-MM-DDTHH:MM:SS`) into RFC3339. Returns `None` on empty or
/// unparseable input.
fn parse_aw_datetime(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Already RFC3339-like (has `+` offset, `-07` at position ≥20, or `Z`).
    if s.ends_with('Z')
        || s.contains('+')
        || (s.len() > 19 && s.chars().nth(19) == Some('-'))
    {
        return Some(s.to_string());
    }
    // Bare `YYYY-MM-DDTHH:MM:SS` — localise.
    use chrono::{NaiveDateTime, TimeZone};
    let ndt = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .ok()?;
    Some(Local.from_local_datetime(&ndt).earliest()?.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Registry face

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(n > 0, || {
                let segs = out.counts.get("segments").copied().unwrap_or(0);
                let accts = out.counts.get("accounts").copied().unwrap_or(0);
                format!("awardwallet synced — {segs} travel segments, {accts} loyalty accounts")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("awardwallet sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let segs = out.counts.get("segments").copied().unwrap_or(0);
    let accts = out.counts.get("accounts").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("AwardWallet synced — {segs} travel segments, {accts} loyalty accounts"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "awardwallet",
        name: "AwardWallet",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your loyalty-program balances and travel reservations from \
                      AwardWallet — 700+ programs including airlines, hotels, and credit \
                      cards. Balances are snapshotted on each sync; flight, hotel, and \
                      car reservations flow into the unified travel timeline.",
        domain: "travel",
        vault_path: "travel/awardwallet/",
        toggleable: true,
        setup: &[
            "Register a developer or Business account at business.awardwallet.com.",
            "Copy your API key from business.awardwallet.com/profile/api and paste it below.",
            "Full travel-timeline export (flights, hotels, cars) requires a paid Business subscription.",
        ],
        caveats: "Requires an AwardWallet Business account and API key. Balance snapshots \
                  are captured on each sync; the raw earn/burn history is limited to the \
                  last 10 transactions per account from the API. The full travel timeline \
                  (flights and hotel bookings) needs a paid Business subscription — a free \
                  developer account yields balances only.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(AWARDWALLET_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("awardwallet"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste: paste the API key.

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let key = raw.trim().to_string();
    if key.is_empty() {
        bail!(
            "empty API key — copy it from business.awardwallet.com/profile/api"
        );
    }
    // Probe: lightweight call to list connected users to verify the key.
    let client = AwClient::new(API_BASE.to_string(), key.clone());
    match client.connected_user() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "AwardWallet rejected the key (401) — copy it fresh from \
             business.awardwallet.com/profile/api"
        ),
        Err(FetchError::Forbidden) => bail!(
            "AwardWallet key is valid but the account lacks Business API access (403)"
        ),
        // Transient errors: store the key anyway so the user isn't blocked.
        Err(e) => eprintln!("awardwallet auth probe: {e} (stored key anyway)"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: key,
            refresh_token: None,
            token_type: None,
            scope: None,
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
        let cursor = load_cursor(vault);
        let mut extra = BTreeMap::new();
        if let Some(d) = cursor.last_balance_date {
            extra.insert("last_balance_date", d);
        }
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "AwardWallet Business Account".to_string(),
            connected_at: None,
            expires_at: ts.expires_at,
            needs_reconnect: false,
            extra,
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "awardwallet",
    display_name: "AwardWallet",
    methods: &[ConnectMethod::TokenPaste {
        label: "AwardWallet API Key",
        help: "Copy your API key from business.awardwallet.com/profile/api. \
               A paid Business subscription is required for the full travel timeline; \
               a free developer account returns loyalty balances only.",
        placeholder: "your-awardwallet-api-key",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["awardwallet"],
    setup: &[
        "Go to business.awardwallet.com and register a developer or Business account.",
        "Navigate to Profile → API and copy your API key.",
        "Paste the key in the connect card. A paid Business subscription unlocks the full travel timeline.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP client

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Forbidden,
    RateLimited,
    NotFound,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Forbidden => write!(f, "forbidden (HTTP 403) — paid Business account required"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FetchError {}

trait AwApi {
    /// `GET /api/export/v1/connectedUser` — returns the list of connected users
    /// (used as a lightweight auth probe and to get user ids).
    fn connected_user(&self) -> Result<Value, FetchError>;

    /// `GET /api/export/v1/account/{id}` — returns the raw account JSON verbatim.
    /// Callers parse a typed copy for field extraction; the original Value is
    /// written to the raw layer unchanged (full-fidelity guarantee).
    fn account(&self, id: i64) -> Result<Value, FetchError>;

    /// `POST /api/export/v1/travel-timeline/{id}` — returns itinerary items.
    fn travel_timeline(
        &self,
        id: i64,
        start: &str,
        end: &str,
        page_token: Option<&str>,
    ) -> Result<Value, FetchError>;
}

struct AwClient {
    base: String,
    api_key: String,
}

impl AwClient {
    fn new(base: String, api_key: String) -> Self {
        AwClient { base, api_key }
    }

    fn get(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("X-Authentication", &self.api_key)
            .set("Accept", "application/json")
            .call();
        map_response(resp)
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value, FetchError> {
        let url = format!("{}{}", self.base, path);
        let resp = ureq::post(&url)
            .timeout(HTTP_TIMEOUT)
            .set("X-Authentication", &self.api_key)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_json(body.clone());
        map_response(resp)
    }
}

fn map_response(resp: Result<ureq::Response, ureq::Error>) -> Result<Value, FetchError> {
    match resp {
        Ok(r) => r.into_json::<Value>().map_err(|e| FetchError::Other(e.to_string())),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
        Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(c, r)) => Err(FetchError::Other(format!(
            "HTTP {c}: {}",
            r.into_string().unwrap_or_default()
        ))),
        Err(ureq::Error::Transport(t)) => Err(FetchError::Other(t.to_string())),
    }
}

impl AwApi for AwClient {
    fn connected_user(&self) -> Result<Value, FetchError> {
        self.get("/api/export/v1/connectedUser")
    }

    fn account(&self, id: i64) -> Result<Value, FetchError> {
        self.get(&format!("/api/export/v1/account/{id}"))
    }

    fn travel_timeline(
        &self,
        id: i64,
        start: &str,
        end: &str,
        page_token: Option<&str>,
    ) -> Result<Value, FetchError> {
        let mut body = serde_json::json!({ "start": start, "end": end });
        if let Some(pt) = page_token {
            body["pageToken"] = Value::String(pt.to_string());
        }
        self.post_json(&format!("/api/export/v1/travel-timeline/{id}"), &body)
    }
}

// ---------------------------------------------------------------------------
// Pull logic

/// Internal outcome type.
struct PullCounts {
    segments: u64,
    accounts: u64,
    duplicates: u64,
    errors: u64,
}

impl PullCounts {
    fn into_map(self) -> BTreeMap<&'static str, u64> {
        BTreeMap::from([
            ("segments", self.segments),
            ("accounts", self.accounts),
            ("duplicates", self.duplicates),
            ("errors", self.errors),
        ])
    }
}

/// Main pull entry point. Called from both the periodic collect pass and the
/// manual "Sync now" hook. Handles missing tokens, rate limits, and
/// 403/paid-tier gracefully.
fn pull(vault: &Vault) -> Result<PullOutcome> {
    let api_key = vault
        .load_sync_token(SERVICE)?
        .context("AwardWallet is not connected")?
        .access_token;

    let client = AwClient::new(API_BASE.to_string(), api_key);
    pull_with(&client, vault)
}

fn pull_with(api: &impl AwApi, vault: &Vault) -> Result<PullOutcome> {
    let mut cursor = load_cursor(vault);
    let mut counts = PullCounts { segments: 0, accounts: 0, duplicates: 0, errors: 0 };

    // Step 1: resolve user id.
    let user_val = api.connected_user().context("failed to list connected users")?;
    let user_id = extract_user_id(&user_val).unwrap_or_else(|| cursor.user_id.unwrap_or(0));
    if user_id == 0 {
        bail!("AwardWallet returned no user id — check your API key and account");
    }
    cursor.user_id = Some(user_id);

    // Step 2: fetch loyalty account balances.
    match api.account(user_id) {
        Ok(raw_val) => {
            // Surface errorCode != 0: non-zero means the balance may be stale
            // or the scrape failed.  We still write the raw object (full
            // fidelity) but log a warning so it's visible in the output.
            let error_code = raw_val
                .get("errorCode")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if error_code != 0 {
                eprintln!(
                    "awardwallet: account {} returned errorCode={} — \
                     balance may be stale or incomplete",
                    user_id, error_code
                );
            }
            write_balance_raw(vault, raw_val)?;
            counts.accounts += 1;
        }
        Err(FetchError::NotFound) => {
            // Account id may vary by API tier — not fatal.
        }
        Err(e) => {
            eprintln!("awardwallet balance fetch error: {e}");
            counts.errors += 1;
        }
    }
    let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
    cursor.last_balance_date = Some(today.clone());

    // Step 3: fetch travel timeline (paid tier; degrade on 403).
    let timeline_start = cursor
        .timeline_since
        .clone()
        .unwrap_or_else(|| "2000-01-01".to_string());
    let end_date = today.clone();

    let (new_segs, dups, parse_errs) =
        pull_timeline(api, vault, user_id, &timeline_start, &end_date)?;
    counts.segments += new_segs;
    counts.duplicates += dups;
    counts.errors += parse_errs;

    // Advance cursor only when zero parse errors: if any itinerary failed to
    // deserialize, we leave the watermark in place so a future pull (after a
    // code fix) can re-collect the dropped items.
    if parse_errs == 0 {
        cursor.timeline_since = Some(today);
    } else {
        eprintln!(
            "awardwallet: {parse_errs} itinerary parse error(s) — \
             cursor NOT advanced; they will be retried on the next sync"
        );
    }
    save_cursor(vault, &cursor)?;

    Ok(PullOutcome {
        headline: format!(
            "AwardWallet synced — {} travel segments, {} loyalty accounts",
            counts.segments, counts.accounts
        ),
        counts: counts.into_map(),
    })
}

/// Fetch all pages of the travel timeline, write raw + contract layers.
/// Returns `(new_segments, duplicate_skips, parse_errors)`.
/// When `parse_errors > 0` the caller should NOT advance the cursor watermark
/// so a future pull (after a code fix) can re-collect the dropped itineraries.
fn pull_timeline(
    api: &impl AwApi,
    vault: &Vault,
    user_id: i64,
    start: &str,
    end: &str,
) -> Result<(u64, u64, u64)> {
    // Load already-seen guids for dedup.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    if let Ok(parts) = stream.partitions() {
        for key in parts {
            if let Ok(segs) = stream.read::<Segment>(&key) {
                for s in segs {
                    if !s.guid.is_empty() {
                        seen.insert(s.guid);
                    }
                }
            }
        }
    }

    let mut page_token: Option<String> = None;
    let mut new_count = 0u64;
    let mut dup_count = 0u64;
    let mut parse_errors = 0u64;
    let mut raw_by_year: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    loop {
        let resp = match api.travel_timeline(user_id, start, end, page_token.as_deref()) {
            Ok(v) => v,
            Err(FetchError::Forbidden) => {
                // Free-tier account — no travel timeline. Degrade gracefully.
                eprintln!(
                    "awardwallet: travel-timeline endpoint returned 403 — \
                     a paid Business subscription is required; skipping timeline"
                );
                break;
            }
            Err(FetchError::NotFound) => break,
            Err(e) => return Err(anyhow::anyhow!("travel-timeline error: {e}")),
        };

        let itineraries = resp
            .get("itineraries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if itineraries.is_empty() {
            break;
        }

        let mut new_segments: Vec<Segment> = Vec::new();
        for raw_val in &itineraries {
            // Capture raw object verbatim.
            let raw_map = raw_val.as_object().cloned().unwrap_or_default();
            let year = raw_map
                .get("reservationDate")
                .and_then(|v| v.as_str())
                .and_then(|d| d.get(..4))
                .unwrap_or("unknown")
                .to_string();
            raw_by_year.entry(year).or_default().push(raw_val.clone());

            // Parse and map to contract segments.
            let itin: AwItinerary = match serde_json::from_value(raw_val.clone()) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("awardwallet: failed to parse itinerary: {e}");
                    parse_errors += 1;
                    continue;
                }
            };
            for seg in itinerary_to_segments(&itin, &raw_map) {
                if seen.insert(seg.guid.clone()) {
                    new_segments.push(seg);
                    new_count += 1;
                } else {
                    dup_count += 1;
                }
            }
        }

        if !new_segments.is_empty() {
            stream.append(&new_segments, |s| &s.ts)?;
        }

        // Pagination.
        page_token = resp
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if page_token.is_none() {
            break;
        }
    }

    // Write raw layer unconditionally (even when no segments mapped).
    for (year, objs) in raw_by_year {
        let rel = format!("{RAW_DIR}/{year}.jsonl");
        let path = vault.resolve(&rel)?;
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut body = String::new();
        for obj in objs {
            body.push_str(&serde_json::to_string(&obj)?);
            body.push('\n');
        }
        write_atomic(&path, body.as_bytes())?;
    }

    Ok((new_count, dup_count, parse_errors))
}

/// Write a balance snapshot object to the raw-balance layer.
/// Partitioned by the current month (YYYY-MM). One row per account per month;
/// a re-pull in the same month replaces the previous snapshot for that account.
///
/// `raw_val` is the **original** JSON `Value` returned by the API — it is
/// written verbatim to preserve full fidelity (no fields are dropped).  Only
/// `accountId` is read for the per-account dedup line, mirroring the
/// reservation path.
fn write_balance_raw(vault: &Vault, raw_val: Value) -> Result<()> {
    let month = Local::now().format("%Y-%m").to_string();
    let rel = format!("{RAW_BAL_DIR}/{month}.jsonl");
    let path = vault.resolve(&rel)?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }

    // Extract accountId from the raw value for per-account dedup.
    let account_id = raw_val
        .get("accountId")
        .and_then(|id| id.as_i64())
        .unwrap_or(0);

    // Read existing lines; update or append this account's line.
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            // Drop any prior line for this account.
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                v.get("accountId").and_then(|id| id.as_i64()) != Some(account_id)
            } else {
                true // keep unparseable lines
            }
        })
        .map(|l| l.to_string())
        .collect();

    // Serialise the original raw value — not a re-serialised typed struct.
    let row = serde_json::to_string(&raw_val)?;
    lines.push(row);
    let content = lines.join("\n") + "\n";
    write_atomic(&path, content.as_bytes())?;
    Ok(())
}

/// Extract a user id from a `/connectedUser` response. The response is an
/// array of connected-user objects, each with a `userId` field.
fn extract_user_id(val: &Value) -> Option<i64> {
    // Response can be { "connectedUsers": [...] } or a bare array.
    let arr = val
        .get("connectedUsers")
        .and_then(|v| v.as_array())
        .or_else(|| val.as_array())?;
    arr.first().and_then(|u| u.get("userId")).and_then(|id| id.as_i64())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-awardwallet-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ----- Documented API shapes (from awardwallet.com/api/account, 2026-06-17) -----

    fn sample_flight_itinerary() -> Value {
        serde_json::json!({
            "type": "flight",
            "status": "confirmed",
            "reservationDate": "2026-09-15",
            "cancelled": false,
            "confirmationNumbers": [{"number": "ABCD12", "isPrimary": true, "description": "PNR"}],
            "segments": [
                {
                    "departure": {
                        "airportCode": "SFO",
                        "name": "San Francisco International",
                        "terminal": "3",
                        "localDateTime": "2026-09-15T08:25:00"
                    },
                    "arrival": {
                        "airportCode": "ORD",
                        "name": "O'Hare International",
                        "terminal": "1",
                        "localDateTime": "2026-09-15T14:40:00"
                    },
                    "marketingCarrier": {
                        "airline": { "name": "United Airlines", "iataCode": "UA" },
                        "flightNumber": "UA 523",
                        "confirmationNumber": "ABCD12"
                    },
                    "aircraft": { "iataCode": "738", "name": "Boeing 737-800" },
                    "cabin": "Economy",
                    "status": "confirmed",
                    "duration": "PT3H15M"
                }
            ],
            "travelers": [{ "firstName": "Jane", "lastName": "Doe" }],
            "pricingInfo": {
                "total": 350.0,
                "currencyCode": "USD"
            }
        })
    }

    fn sample_hotel_itinerary() -> Value {
        serde_json::json!({
            "type": "hotelReservation",
            "status": "confirmed",
            "reservationDate": "2026-09-20",
            "cancelled": false,
            "confirmationNumbers": [{"number": "HOTEL-9988", "isPrimary": true}],
            "hotelName": "Marriott Chicago Downtown",
            "chainName": "Marriott",
            "address": { "city": "Chicago", "country": "US" },
            "checkInDate": "2026-09-20",
            "checkOutDate": "2026-09-22",
            "guestCount": 2,
            "roomsCount": 1,
            "pricingInfo": { "total": 420.0, "currencyCode": "USD" }
        })
    }

    fn sample_car_itinerary() -> Value {
        serde_json::json!({
            "type": "carRental",
            "status": "confirmed",
            "reservationDate": "2026-09-20",
            "cancelled": false,
            "confirmationNumbers": [{"number": "CAR-5678", "isPrimary": true}],
            "rentalCompany": "Hertz",
            "pickup": {
                "locationName": "ORD Airport",
                "localDateTime": "2026-09-20T15:00:00"
            },
            "dropoff": {
                "locationName": "MDW Airport",
                "localDateTime": "2026-09-22T09:00:00"
            }
        })
    }

    fn sample_account() -> Value {
        serde_json::json!({
            "accountId": 12345,
            "code": "AA",
            "displayName": "American Airlines AAdvantage",
            "kind": "Airlines",
            "login": "jane@example.com",
            "balance": "45,200 Miles",
            "balanceRaw": 45200,
            "owner": "Jane Doe",
            "errorCode": 1,
            "lastRetrieveDate": "2026-06-17T10:00:00Z",
            "history": [],
            "properties": [
                { "kind": 3, "name": "Status", "value": "Gold" },
                { "kind": 12, "name": "Name", "value": "Jane Doe" }
            ]
        })
    }

    // ----- itinerary_to_segments tests -----

    #[test]
    fn flight_itinerary_maps_to_a_flight_segment() {
        let raw = sample_flight_itinerary();
        let itin: AwItinerary = serde_json::from_value(raw.clone()).unwrap();
        let raw_map = raw.as_object().unwrap().clone();
        let segs = itinerary_to_segments(&itin, &raw_map);
        assert_eq!(segs.len(), 1, "one leg → one segment");
        let s = &segs[0];
        assert_eq!(s.source, "awardwallet");
        assert_eq!(s.type_, "flight");
        assert_eq!(s.start_place, "SFO");
        assert_eq!(s.start_place_name, "San Francisco International");
        assert_eq!(s.end_place, "ORD");
        assert_eq!(s.vendor, "United Airlines");
        assert_eq!(s.number, "UA 523");
        assert_eq!(s.confirmation, "ABCD12");
        assert_eq!(s.booking_id, "ABCD12");
        assert_eq!(s.status, "confirmed");
        assert!(!s.ts.is_empty(), "ts is set from departure localDateTime");
        assert!(!s.end_ts.is_empty(), "end_ts from arrival localDateTime");
        assert_eq!(
            s.extra.get("cabin"),
            Some(&Value::String("Economy".into())),
            "cabin in extra"
        );
        assert_eq!(
            s.extra.get("departure_terminal"),
            Some(&Value::String("3".into())),
            "departure_terminal in extra"
        );
    }

    #[test]
    fn hotel_itinerary_maps_to_a_lodging_segment() {
        let raw = sample_hotel_itinerary();
        let itin: AwItinerary = serde_json::from_value(raw.clone()).unwrap();
        let raw_map = raw.as_object().unwrap().clone();
        let segs = itinerary_to_segments(&itin, &raw_map);
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        assert_eq!(s.type_, "lodging");
        assert_eq!(s.start_place_name, "Marriott Chicago Downtown");
        assert_eq!(s.start_place, "Chicago");
        assert_eq!(s.vendor, "Marriott");
        assert_eq!(s.confirmation, "HOTEL-9988");
        assert!(!s.ts.is_empty(), "ts is set from checkInDate");
        assert!(!s.end_ts.is_empty(), "end_ts from checkOutDate");
    }

    #[test]
    fn car_itinerary_maps_to_a_car_segment() {
        let raw = sample_car_itinerary();
        let itin: AwItinerary = serde_json::from_value(raw.clone()).unwrap();
        let raw_map = raw.as_object().unwrap().clone();
        let segs = itinerary_to_segments(&itin, &raw_map);
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        assert_eq!(s.type_, "car");
        assert_eq!(s.vendor, "Hertz");
        assert_eq!(s.confirmation, "CAR-5678");
        assert_eq!(s.start_place_name, "ORD Airport");
        assert_eq!(s.end_place_name, "MDW Airport");
        assert!(!s.ts.is_empty());
    }

    #[test]
    fn cancelled_itinerary_gets_cancelled_status() {
        let mut raw = sample_hotel_itinerary();
        raw["cancelled"] = Value::Bool(true);
        let itin: AwItinerary = serde_json::from_value(raw.clone()).unwrap();
        let segs = itinerary_to_segments(&itin, &raw.as_object().unwrap().clone());
        assert!(!segs.is_empty());
        assert_eq!(segs[0].status, "cancelled");
    }

    #[test]
    fn unknown_itinerary_type_returns_empty() {
        let raw = serde_json::json!({
            "type": "cruise",
            "status": "confirmed",
            "reservationDate": "2026-10-01"
        });
        let itin: AwItinerary = serde_json::from_value(raw.clone()).unwrap();
        let segs = itinerary_to_segments(&itin, &raw.as_object().unwrap().clone());
        assert!(segs.is_empty(), "unknown types return empty — no silent data loss");
    }

    // ----- parse_aw_datetime -----

    #[test]
    fn parse_aw_datetime_handles_various_formats() {
        // RFC3339 with Z — pass-through.
        let rfc = "2026-09-15T08:25:00Z";
        assert_eq!(parse_aw_datetime(rfc), Some(rfc.to_string()));
        // RFC3339 with offset — pass-through.
        let off = "2026-09-15T08:25:00-07:00";
        assert_eq!(parse_aw_datetime(off), Some(off.to_string()));
        // Bare local datetime — localised to RFC3339.
        let ts = parse_aw_datetime("2026-09-15T08:25:00");
        assert!(ts.is_some());
        assert!(ts.unwrap().starts_with("2026-09-15T08:25:00"), "localised correctly");
        // Empty → None.
        assert!(parse_aw_datetime("").is_none());
        // Unparseable → None.
        assert!(parse_aw_datetime("not-a-date").is_none());
    }

    // ----- Raw-layer write -----

    #[test]
    fn balance_raw_write_creates_file_and_is_idempotent() {
        let v = temp_vault("balance-raw");
        let raw = sample_account();
        write_balance_raw(&v, raw.clone()).unwrap();
        let month = Local::now().format("%Y-%m").to_string();
        let path = v.root().join(format!("travel/awardwallet/raw/balances/{month}.jsonl"));
        assert!(path.exists(), "balance raw file created");
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"accountId\":12345"), "account id present: {content}");
        assert!(content.contains("AAdvantage"), "displayName present");
        // Full-fidelity: fields not on AwAccount struct must still be present.
        assert!(content.contains("errorCode"), "errorCode present in raw output (fidelity)");

        // Re-run: same account should NOT produce a second line.
        write_balance_raw(&v, raw).unwrap();
        let content2 = fs::read_to_string(&path).unwrap();
        let line_count = content2.lines().filter(|l| !l.trim().is_empty()).count();
        assert_eq!(line_count, 1, "idempotent: same account → still one line");
    }

    // ----- Cursor persistence -----

    #[test]
    fn cursor_round_trips() {
        let v = temp_vault("cursor");
        let c = Cursor {
            user_id: Some(999),
            last_balance_date: Some("2026-06-17".into()),
            timeline_since: Some("2026-06-01".into()),
        };
        save_cursor(&v, &c).unwrap();
        let c2 = load_cursor(&v);
        assert_eq!(c2.user_id, Some(999));
        assert_eq!(c2.last_balance_date.as_deref(), Some("2026-06-17"));
        assert_eq!(c2.timeline_since.as_deref(), Some("2026-06-01"));
    }

    // ----- extract_user_id -----

    #[test]
    fn extract_user_id_from_connected_user_response() {
        let resp = serde_json::json!({
            "connectedUsers": [
                { "userId": 42, "fullName": "Jane Doe", "email": "jane@example.com" }
            ]
        });
        assert_eq!(extract_user_id(&resp), Some(42));
        // Bare array form.
        let bare = serde_json::json!([{ "userId": 7 }]);
        assert_eq!(extract_user_id(&bare), Some(7));
        // Empty → None.
        let empty = serde_json::json!({ "connectedUsers": [] });
        assert_eq!(extract_user_id(&empty), None);
    }

    // ----- Offline pull simulation (via trait injection) -----

    struct FakeAwApi {
        accounts: BTreeMap<i64, Result<Value, ()>>,
        timeline: Vec<Value>,
        user_id: i64,
    }

    impl AwApi for FakeAwApi {
        fn connected_user(&self) -> Result<Value, FetchError> {
            Ok(serde_json::json!({ "connectedUsers": [{ "userId": self.user_id }] }))
        }

        fn account(&self, id: i64) -> Result<Value, FetchError> {
            match self.accounts.get(&id) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(())) => Err(FetchError::Forbidden),
                None => Err(FetchError::NotFound),
            }
        }

        fn travel_timeline(
            &self,
            _id: i64,
            _start: &str,
            _end: &str,
            _page_token: Option<&str>,
        ) -> Result<Value, FetchError> {
            Ok(serde_json::json!({ "itineraries": self.timeline }))
        }
    }

    #[test]
    fn offline_pull_writes_segments_and_raw() {
        let v = temp_vault("offline-pull");
        let api = FakeAwApi {
            user_id: 42,
            accounts: BTreeMap::from([(42, Ok(sample_account()))]),
            timeline: vec![
                sample_flight_itinerary(),
                sample_hotel_itinerary(),
                sample_car_itinerary(),
            ],
        };
        let out = pull_with(&api, &v).unwrap();
        assert_eq!(out.counts.get("segments"), Some(&3u64), "3 segments from 3 itineraries");
        assert_eq!(out.counts.get("accounts"), Some(&1u64));

        // Raw reservation file for 2026 should exist.
        let raw_path = v.root().join("travel/awardwallet/raw/2026.jsonl");
        assert!(raw_path.exists(), "raw reservation file created");
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_content.lines().count(), 3, "3 raw objects");

        // Contract travel stream should have 3 rows.
        let stream = v.stream(DIR, Partition::Month);
        let parts = stream.partitions().unwrap();
        let total: usize = parts.iter().map(|k| stream.read::<Segment>(k).unwrap().len()).sum();
        assert_eq!(total, 3, "3 contract Segment rows in vault");
    }

    #[test]
    fn offline_pull_is_idempotent_on_rerun() {
        let v = temp_vault("idempotent");
        let api = FakeAwApi {
            user_id: 42,
            accounts: BTreeMap::from([(42, Ok(sample_account()))]),
            timeline: vec![sample_flight_itinerary()],
        };
        let out1 = pull_with(&api, &v).unwrap();
        let out2 = pull_with(&api, &v).unwrap();
        assert_eq!(out1.counts.get("segments"), Some(&1u64));
        // Second run: same guid → duplicate, no new segment.
        assert_eq!(out2.counts.get("segments"), Some(&0u64));
        assert_eq!(out2.counts.get("duplicates"), Some(&1u64));
    }

    #[test]
    fn forbidden_timeline_degrades_gracefully() {
        let v = temp_vault("forbidden");

        struct ForbiddenApi;
        impl AwApi for ForbiddenApi {
            fn connected_user(&self) -> Result<Value, FetchError> {
                Ok(serde_json::json!({ "connectedUsers": [{ "userId": 1 }] }))
            }
            fn account(&self, _: i64) -> Result<Value, FetchError> {
                Ok(serde_json::json!({
                    "accountId": 1,
                    "code": "HH",
                    "displayName": "Hilton Honors",
                    "balance": "10,000 Points",
                    "errorCode": 0
                }))
            }
            fn travel_timeline(
                &self, _: i64, _: &str, _: &str, _: Option<&str>,
            ) -> Result<Value, FetchError> {
                Err(FetchError::Forbidden)
            }
        }

        // Pull should succeed even though the timeline endpoint returns 403.
        let out = pull_with(&ForbiddenApi, &v).unwrap();
        assert_eq!(out.counts.get("segments"), Some(&0u64), "no segments on 403");
        assert_eq!(out.counts.get("accounts"), Some(&1u64), "balance still collected");
    }

    // ----- DEF / CONNECTION shape checks -----

    #[test]
    fn def_is_periodic_on_travel_domain() {
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert_eq!(DEF.meta.domain, "travel");
        assert_eq!(DEF.meta.vault_path, "travel/awardwallet/");
        assert_eq!(DEF.meta.id, "awardwallet");
        assert!(!DEF.meta.default_on, "loyalty data is opt-in");
        assert_eq!(DEF.connection, Some("awardwallet"), "references its own connection");
    }

    #[test]
    fn connection_uses_token_paste() {
        assert_eq!(CONNECTION.id, "awardwallet");
        assert_eq!(CONNECTION.methods.len(), 1);
        assert!(
            matches!(CONNECTION.methods[0], ConnectMethod::TokenPaste { .. }),
            "API key, not OAuth"
        );
    }
}
