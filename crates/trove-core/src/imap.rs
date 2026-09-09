//! Generic IMAP email collector — any IMAP-capable mailbox (iCloud Mail,
//! Yahoo, Zoho, Fastmail, GMX, AOL, a self-hosted/custom-domain server) into
//! the unified correspondence stream. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/imap.md.
//!
//! **Target.** Messages land in `correspondence/email/YYYY-MM.jsonl` (see
//! [`crate::correspondence`]) — the *same* stream the `.mbox` importer
//! ([`crate::email`]) and the Gmail puller ([`crate::gmail`]) write, sharing
//! their Message-ID dedupe. So the same message reached three ways (Takeout
//! mbox, Gmail API, and IMAP) is stored once. This is essentially "gmail.rs
//! over IMAP": the Gmail-API fetch is swapped for an `UID FETCH BODY.PEEK[]`,
//! and the verbatim RFC822 bytes go through the *exact same*
//! [`crate::email::email_to_message`] parse. `service` is the account address,
//! `labels` carry the IMAP folder name.
//!
//! **Transport.** A SYNC IMAP client (the [`crate::registry`] pull hooks are
//! sync `fn(&Vault) -> …`) over **rustls** — pure-Rust TLS, no OpenSSL /
//! native-tls (the standalone rule). We build the `rustls::StreamOwned`
//! ourselves and hand it to the `imap` crate's `Client::new`, so the crate's
//! own `native-tls` default is off. See `Cargo.toml` for the module/crate
//! name-clash note (the extern crate is aliased [`imap_client`]).
//!
//! **Auth (a SECRET).** One pasted [`ConnectMethod::TokenPaste`] field encodes
//! everything: `email app-password [host [port]]` (whitespace-separated). The
//! host is derived from the email domain via a built-in well-known table when
//! omitted (icloud→imap.mail.me.com, yahoo→imap.mail.yahoo.com, …). Connect
//! validates with a real TLS connect + LOGIN, then stores `{host, port,
//! username, password}` as JSON in the secret store (0600 `.trove/sync/`,
//! reusing the [`crate::sync::oauth::TokenSet`] `access_token` slot like Oura's
//! PAT and lastfm's username). The password is a secret — never logged, never
//! in the non-secret cursor.
//!
//! **Cursor (non-secret).** Per-folder `{uidvalidity, max_uid}` in
//! `.trove/imap-sync.json` (the [`crate::lastfm`] cursor placement). On each
//! pass: if the folder's current `UIDVALIDITY` differs from the stored one (or
//! there's no cursor), refetch from UID 1 (guid dedupe absorbs the overlap);
//! otherwise `UID FETCH` UIDs greater than `max_uid`. The cursor advances per
//! folder only after that folder's batch is written (drain-don't-strand).
//!
//! **v1 scope.** A SINGLE mailbox (re-connecting replaces it). Deferred (clean
//! TODOs below): multi-account (single-field TokenPaste can't cleanly express
//! N accounts) and XOAUTH2 (Gmail/Outlook have dedicated providers). Bodies +
//! attachment *metadata* only, mirroring gmail/email — NO raw .eml storage, NO
//! attachment downloads (a later collection-depth opt-in).

// Deferred scope (intentionally not built in v1):
// TODO(imap): multi-account — a single-field TokenPaste doesn't cleanly carry
//   N accounts; would need either a repeatable connect method or a structured
//   form. Gmail/Outlook already have dedicated multi-account providers.
// TODO(imap): XOAUTH2 — app-password LOGIN suffices for iCloud/Yahoo/Zoho/
//   Fastmail/custom domains; Gmail and Outlook (which deprecate app passwords
//   for many accounts) have their own OAuth-based collectors.
// TODO(imap): raw .eml archive + attachment download — a collection-depth
//   opt-in (mirrors gmail.rs), default-off; v1 keeps bodies + attachment
//   metadata only.

use std::collections::{BTreeMap, HashSet};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

// The module is `crate::imap`; alias the extern crate so the two never collide.
use ::imap as imap_client;

use crate::correspondence::Message;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::write_json_atomic;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// The unified email stream — shared with mbox import and Gmail (same folder,
/// same Message-ID dedupe).
const SOURCE: &str = "email";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for 0600
/// secrets); deleting it re-walks every folder from UID 1 next sync.
const SYNC_FILE: &str = ".trove/imap-sync.json";
/// The service id under `.trove/sync/` where the IMAP creds JSON is stored
/// (the Oura-PAT slot: the blob rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "imap";

const DEFAULT_PORT: u16 = 993;
/// Kept short so a hung connection can't stall the watcher owner loop (the
/// gmail/oura reasoning).
const NET_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop. Mail is time-sensitive enough to
/// want sub-hourly freshness, cheap enough (one `UID SEARCH` per folder when
/// nothing changed) to afford it — the gmail cadence.
pub const IMAP_SYNC_SECS: u64 = 900;
/// How many UIDs to `FETCH` per round-trip, so one batch can't pull an entire
/// mailbox into memory at once and a budget/interruption strands at most this
/// many un-cursored messages.
const FETCH_CHUNK: usize = 200;

// ---------------------------------------------------------------------------
// Well-known IMAP hosts, so the user can paste just `email app-password` for
// the common providers. Unknown domains require an explicit host (a clear
// connect error walks them through it). Matched on the email's domain,
// lowercased.

/// `(domain, imap-host)` for the providers an app-password LOGIN works with.
/// Gmail/Outlook are deliberately absent — they have dedicated OAuth
/// collectors, and app passwords are restricted there.
const WELL_KNOWN: &[(&str, &str)] = &[
    ("icloud.com", "imap.mail.me.com"),
    ("me.com", "imap.mail.me.com"),
    ("mac.com", "imap.mail.me.com"),
    ("yahoo.com", "imap.mail.yahoo.com"),
    ("yahoo.co.uk", "imap.mail.yahoo.com"),
    ("ymail.com", "imap.mail.yahoo.com"),
    ("rocketmail.com", "imap.mail.yahoo.com"),
    ("aol.com", "imap.aol.com"),
    ("zoho.com", "imap.zoho.com"),
    ("zohomail.com", "imap.zoho.com"),
    ("fastmail.com", "imap.fastmail.com"),
    ("fastmail.fm", "imap.fastmail.com"),
    ("gmx.com", "imap.gmx.com"),
    ("gmx.net", "imap.gmx.net"),
    ("gmx.de", "imap.gmx.net"),
    ("web.de", "imap.web.de"),
    ("mail.com", "imap.mail.com"),
    ("hey.com", "imap.hey.com"),
    // Proton/Gmail/Outlook are intentionally absent: Proton needs the Bridge
    // (not this collector's job), Gmail/Outlook have dedicated providers. An
    // unknown domain falls through to the clear "append the host" error.
];

// ---------------------------------------------------------------------------
// Connect-field parsing — PURE, fixture-tested.

/// The resolved IMAP credentials. The password is a SECRET: it is only ever
/// written to the 0600 secret store, never to the cursor or a log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImapCreds {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// Parse the single pasted connect field into [`ImapCreds`].
///
/// Format: `email app-password [host [port]]` (whitespace-separated). The
/// password is exactly token 1 — app passwords are alphanumeric (often shown
/// with spaces, e.g. iCloud's `abcd-efgh-ijkl-mnop`, but pasted without), so
/// we don't try to glue spaces back; token 2 (if present) is the host, token 3
/// the port. When the host is omitted it is derived from the email's domain
/// via [`WELL_KNOWN`]; an unknown domain with no explicit host is a clear
/// error telling the user to append the host.
pub(crate) fn parse_connect_field(field: &str) -> Result<ImapCreds> {
    let toks: Vec<&str> = field.split_whitespace().collect();
    let username = match toks.first() {
        Some(u) if u.contains('@') => u.to_string(),
        Some(_) => bail!("the first value must be your full email address (e.g. you@icloud.com)"),
        None => bail!("paste your email and app password: `you@icloud.com app-password`"),
    };
    let password = match toks.get(1) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => bail!(
            "missing app password — paste `{} <app-password>` (create one in your mail \
             provider's account-security settings, not your login password)",
            username
        ),
    };

    let host = match toks.get(2) {
        Some(h) => h.to_string(),
        None => {
            let domain = username.rsplit('@').next().unwrap_or("").to_lowercase();
            match WELL_KNOWN.iter().find(|(d, _)| *d == domain) {
                Some((_, host)) => host.to_string(),
                None => bail!(
                    "unknown provider for {domain:?} — append the IMAP host: \
                     `{username} app-password mail.example.com` (and optionally a port, \
                     default {DEFAULT_PORT})"
                ),
            }
        }
    };

    let port = match toks.get(3) {
        Some(p) => p
            .parse::<u16>()
            .with_context(|| format!("invalid port {p:?} — expected a number like {DEFAULT_PORT}"))?,
        None => DEFAULT_PORT,
    };
    if toks.len() > 4 {
        bail!("too many values — expected `email app-password [host [port]]`");
    }

    Ok(ImapCreds {
        host,
        port,
        username,
        password,
    })
}

// ---------------------------------------------------------------------------
// Per-folder UID cursor — PURE logic, fixture-tested. The network layer asks
// `plan_fetch` what to fetch, then reports the new max UID back via `advanced`.

/// One folder's persisted cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderCursor {
    /// The mailbox's `UIDVALIDITY` when we last synced. If the server's value
    /// changes, every UID is invalidated and we refetch from 1.
    #[serde(default)]
    pub uidvalidity: u32,
    /// The highest UID we've fetched-and-written for this folder.
    #[serde(default)]
    pub max_uid: u32,
}

/// What to fetch from a folder this pass, decided from the stored cursor and
/// the server's *current* `UIDVALIDITY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchPlan {
    /// Stored UIDVALIDITY is gone/changed (or no cursor): the whole mailbox is
    /// new ground — `UID FETCH 1:*` (dedupe absorbs anything already stored).
    FromStart,
    /// Same UIDVALIDITY: only UIDs strictly greater than `after` — `UID
    /// SEARCH UID <after+1>:*`.
    After(u32),
}

impl FetchPlan {
    /// The IMAP UID range/sequence this plan fetches, as the string `UID
    /// SEARCH`/`UID FETCH` want.
    pub(crate) fn uid_range(&self) -> String {
        match self {
            FetchPlan::FromStart => "1:*".to_string(),
            // `n:*` always includes at least the highest existing UID even when
            // n > the max, so an empty delta is handled by the server returning
            // just that one (already-stored) UID, which dedupe drops.
            FetchPlan::After(after) => format!("{}:*", after.saturating_add(1)),
        }
    }
}

/// Decide what to fetch given the stored cursor (if any) and the folder's
/// current server `UIDVALIDITY`. A new folder, or one whose validity changed,
/// refetches from the start; otherwise we pick up after the stored max UID.
pub(crate) fn plan_fetch(stored: Option<&FolderCursor>, current_uidvalidity: u32) -> FetchPlan {
    match stored {
        Some(c) if c.uidvalidity == current_uidvalidity && c.uidvalidity != 0 => {
            FetchPlan::After(c.max_uid)
        }
        _ => FetchPlan::FromStart,
    }
}

/// The cursor to persist after a folder's batch is written: the current
/// `UIDVALIDITY` plus the max of the previously-stored max and the highest UID
/// fetched this pass. Never moves the max backward (a partial/empty fetch under
/// the same validity keeps the old high-water mark).
pub(crate) fn advanced_cursor(
    stored: Option<&FolderCursor>,
    current_uidvalidity: u32,
    fetched_max_uid: Option<u32>,
) -> FolderCursor {
    // When the validity changed we restart the max from this pass's fetch
    // (the old max referred to a now-invalid UID space).
    let prior_max = match stored {
        Some(c) if c.uidvalidity == current_uidvalidity => c.max_uid,
        _ => 0,
    };
    FolderCursor {
        uidvalidity: current_uidvalidity,
        max_uid: prior_max.max(fetched_max_uid.unwrap_or(0)),
    }
}

// ---------------------------------------------------------------------------
// Cursor file (non-secret).

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// Per-folder cursors, keyed by the folder's full IMAP name.
    #[serde(default)]
    pub folders: BTreeMap<String, FolderCursor>,
    /// The connected account address, for the index/UI. Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Total messages ever written by this collector (across folders).
    #[serde(default)]
    pub messages: u64,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    pub(crate) fn read_imap_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_imap_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Bytes → Message → dedup → append. The testable seam shared by the live pull
// and the unit tests (which feed canned RFC822 bytes), reusing the mbox/Gmail
// parser verbatim and the shared `email` dedupe set + append.

/// Convert one folder's raw RFC822 messages into correspondence records,
/// dropping any whose guid is already stored (in `seen`, which is GROWN so a
/// duplicate within the same batch is also caught), and append the survivors.
/// Returns the number written. `seen` should be seeded from
/// [`Vault::correspondence_guids`] so IMAP coexists with mbox/Gmail without
/// duplicating.
fn store_folder_messages(
    vault: &Vault,
    folder: &str,
    account: &str,
    raws: &[Vec<u8>],
    seen: &mut HashSet<String>,
) -> Result<u64> {
    let mut batch: Vec<Message> = Vec::new();
    for raw in raws {
        let Some(mut m) = crate::email::email_to_message(raw, account) else {
            continue; // unparseable / undateable — skip (mirrors mbox import)
        };
        // The IMAP folder is the message's label (a read-time filter), the
        // same role Gmail's labelIds play.
        m.labels = vec![folder.to_string()];
        if seen.insert(m.guid.clone()) {
            batch.push(m);
        }
    }
    let written = batch.len() as u64;
    if !batch.is_empty() {
        vault.append_messages(&batch)?;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// The network layer — thin, behind a trait so the orchestration above is the
// only thing tested (the live socket is validated by David: Needs-login).

/// A live folder's identity + the UIDs present, enough to plan a fetch.
struct FolderState {
    name: String,
    uidvalidity: u32,
}

/// The IMAP operations the pull needs. Kept minimal; the real impl wraps the
/// `imap` crate over a rustls stream.
trait ImapOps {
    /// Selectable folder names (`\Noselect` containers excluded).
    fn folders(&mut self) -> Result<Vec<String>>;
    /// Open a folder read-only (EXAMINE — doesn't clear \Recent) and report its
    /// `UIDVALIDITY`.
    fn examine(&mut self, folder: &str) -> Result<FolderState>;
    /// UIDs in the currently-selected folder matching `range` (e.g. `"1:*"` or
    /// `"42:*"`), ascending.
    fn search_uids(&mut self, range: &str) -> Result<Vec<u32>>;
    /// `UID FETCH <uids> BODY.PEEK[]` for the currently-selected folder →
    /// verbatim RFC822 bytes per message (PEEK leaves \Seen untouched).
    fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<Vec<u8>>>;
}

/// A live IMAP session over rustls.
struct ImapSession {
    session: imap_client::Session<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
}

/// Build a rustls TLS stream to `host:port`. Pure-Rust: `ring` crypto +
/// webpki-roots trust anchors, no OS keychain / OpenSSL.
fn tls_connect(
    host: &str,
    port: u16,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .with_context(|| format!("invalid IMAP host name {host:?}"))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .context("starting TLS session")?;
    let tcp = TcpStream::connect((host, port))
        .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_read_timeout(Some(NET_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(NET_TIMEOUT)).ok();
    Ok(rustls::StreamOwned::new(conn, tcp))
}

impl ImapSession {
    /// TLS connect + LOGIN. The password is consumed here and never logged; a
    /// login failure reports the server's message without echoing creds.
    fn connect(creds: &ImapCreds) -> Result<Self> {
        let tls = tls_connect(&creds.host, creds.port)?;
        let client = imap_client::Client::new(tls);
        let session = client
            .login(&creds.username, &creds.password)
            .map_err(|(e, _client)| {
                anyhow::anyhow!("IMAP login failed for {}: {e}", creds.username)
            })?;
        Ok(ImapSession { session })
    }

    fn logout(&mut self) {
        let _ = self.session.logout();
    }
}

impl ImapOps for ImapSession {
    fn folders(&mut self) -> Result<Vec<String>> {
        let names = self
            .session
            .list(Some(""), Some("*"))
            .context("listing IMAP folders")?;
        Ok(names
            .iter()
            .filter(|n| {
                !n.attributes()
                    .iter()
                    .any(|a| matches!(a, imap_client::types::NameAttribute::NoSelect))
            })
            .map(|n| n.name().to_string())
            .collect())
    }

    fn examine(&mut self, folder: &str) -> Result<FolderState> {
        let mailbox = self
            .session
            .examine(folder)
            .with_context(|| format!("EXAMINE {folder}"))?;
        Ok(FolderState {
            name: folder.to_string(),
            // A server without UIDVALIDITY is degenerate; treat as 0, which
            // `plan_fetch` reads as "no stable UID space" → fetch from start.
            uidvalidity: mailbox.uid_validity.unwrap_or(0),
        })
    }

    fn search_uids(&mut self, range: &str) -> Result<Vec<u32>> {
        let set = self
            .session
            .uid_search(format!("UID {range}"))
            .with_context(|| format!("UID SEARCH {range}"))?;
        let mut uids: Vec<u32> = set.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<Vec<u8>>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let fetches = self
            .session
            .uid_fetch(set, "BODY.PEEK[]")
            .context("UID FETCH BODY.PEEK[]")?;
        Ok(fetches
            .iter()
            .filter_map(|f| f.body().map(|b| b.to_vec()))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// The pull body over `ImapOps` — the testable orchestration seam.

/// Sync every folder of one mailbox into the email stream, advancing each
/// folder's cursor only after its batch is written. Returns messages written
/// this pass.
fn pull_with(
    vault: &Vault,
    ops: &mut impl ImapOps,
    account: &str,
) -> Result<u64> {
    let mut state = vault.read_imap_sync();
    state.account = Some(account.to_string());
    // The shared email dedupe set, loaded once and grown as we write — so IMAP
    // coexists with mbox imports and Gmail pulls and re-runs never duplicate.
    let mut seen = vault.correspondence_guids(SOURCE)?;
    let mut written_total: u64 = 0;

    let folders = ops.folders()?;
    for folder in folders {
        let fs = ops.examine(&folder)?;
        let plan = plan_fetch(state.folders.get(&fs.name), fs.uidvalidity);
        let uids = ops.search_uids(&plan.uid_range())?;
        // Under an incremental plan the server's `n:*` always returns the
        // single highest existing UID even when nothing is newer; drop UIDs we
        // already cursored so an idle folder fetches nothing.
        let uids: Vec<u32> = match plan {
            FetchPlan::After(after) => uids.into_iter().filter(|u| *u > after).collect(),
            FetchPlan::FromStart => uids,
        };

        let mut folder_written: u64 = 0;
        let mut max_fetched: Option<u32> = None;
        // Fetch in chunks so a huge folder doesn't balloon memory; the cursor
        // moves only after the WHOLE folder drains, so an interruption mid-
        // folder re-fetches the rest next pass (dedupe absorbs the overlap)
        // rather than stranding it behind an advanced cursor.
        for chunk in uids.chunks(FETCH_CHUNK) {
            let raws = ops.fetch_bodies(chunk)?;
            folder_written += store_folder_messages(vault, &fs.name, account, &raws, &mut seen)?;
            if let Some(&hi) = chunk.iter().max() {
                max_fetched = Some(max_fetched.map_or(hi, |m: u32| m.max(hi)));
            }
        }

        // Advance the cursor for this folder now its batch is on disk.
        let cursor = advanced_cursor(state.folders.get(&fs.name), fs.uidvalidity, max_fetched);
        state.folders.insert(fs.name.clone(), cursor);
        state.messages += folder_written;
        written_total += folder_written;
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_imap_sync(&state)?;
    write_index(vault, &state)?;
    Ok(written_total)
}

/// A human-readable summary at `correspondence/email/imap.md`.
fn write_index(vault: &Vault, state: &SyncState) -> Result<()> {
    let mut md = format!(
        "# IMAP\n\nAccount: {}\nLast sync: {}\nMessages: {}\n\n| Folder | UIDVALIDITY | Max UID |\n|---|---|---|\n",
        state.account.as_deref().unwrap_or("—"),
        state.updated.as_deref().unwrap_or("—"),
        state.messages,
    );
    for (name, c) in &state.folders {
        md.push_str(&format!("| {name} | {} | {} |\n", c.uidvalidity, c.max_uid));
    }
    let path = vault.resolve("correspondence/email/imap.md")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::store::write_atomic(&path, md.as_bytes())
}

/// Resolve creds + sync. Missing creds ⇒ a quiet `Ok(0)` (the lastfm/gmail
/// missing-creds path: the periodic pass stays silent, the manual pull turns
/// it into a clear error).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let creds = match load_creds(vault)? {
        Some(c) => c,
        None => bail!("IMAP is not connected — add your email + app password in the Integrations tab"),
    };
    let mut session = ImapSession::connect(&creds)?;
    let result = pull_with(vault, &mut session, &creds.username);
    session.logout();
    let written = result?;
    Ok(PullOutcome {
        headline: if written == 0 {
            "IMAP is up to date — no new messages".to_string()
        } else {
            format!("IMAP synced — {written} new messages")
        },
        counts: BTreeMap::from([("messages", written)]),
    })
}

/// Load the stored creds JSON out of the secret store. `None` = not connected.
fn load_creds(vault: &Vault) -> Result<Option<ImapCreds>> {
    let Some(token) = vault.load_sync_token(SERVICE)? else {
        return Ok(None);
    };
    let blob = token.access_token;
    if blob.trim().is_empty() {
        return Ok(None);
    }
    let creds: ImapCreds =
        serde_json::from_str(&blob).context("parsing stored IMAP credentials")?;
    Ok(Some(creds))
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join("correspondence/email"))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// missing creds or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    // A quiet no-op when not connected (don't even attempt a TLS connect).
    if load_creds(vault)?.is_none() {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("messages").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("imap synced — {n} messages")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "imap sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (replaces the NotWired
/// stub).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "imap",
        name: "IMAP Email (any provider)",
        kind: IntegrationKind::CloudSync,
        // Opt-in: pulling full message bodies is a privacy-sensitive choice,
        // and most users won't have an IMAP account beyond Gmail/Outlook
        // (which have dedicated collectors).
        default_on: false,
        description: "Pulls email from any IMAP account — iCloud Mail, Yahoo, Zoho, \
                      Fastmail, or your own domain — using an app password, into the same \
                      email stream as Gmail and mbox imports (deduped by Message-ID). \
                      One collector covers every IMAP provider.",
        domain: "correspondence",
        vault_path: "correspondence/email/",
        toggleable: true,
        setup: &[
            "Connect on the card above with your email and an app-specific password (NOT your login password).",
            "iCloud example: appleid.apple.com → Sign-In & Security → App-Specific Passwords → generate one; the host imap.mail.me.com is filled in for you.",
            "For a provider Trove doesn't know, append the IMAP host: `you@example.com app-password mail.example.com`.",
        ],
        caveats: "Reads full message bodies from your mailbox — connect early, because mail \
                  the server has already deleted is gone before Trove can save it. \
                  Bodies and attachment metadata are stored; raw .eml archives and attachment \
                  files are not (a later opt-in). v1 syncs a single mailbox; Gmail and Outlook \
                  have their own dedicated collectors.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(IMAP_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("imap"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a composite single field, a SECRET).

/// Parse the field, do a real TLS connect + LOGIN to validate, then store the
/// creds as JSON in the secret store (0600). v1 = single mailbox: re-connecting
/// replaces the creds and (because the cursor's account changes) the next sync
/// re-walks. The password never leaves the secret store.
fn def_connect(vault: &Vault, field: &str) -> Result<()> {
    let creds = parse_connect_field(field)?;
    // Validate with a live login — a clear error reaches the connect UI.
    let mut session = ImapSession::connect(&creds)?;
    session.logout();

    // Store the whole creds blob in the access_token slot (the Oura-PAT idiom).
    let blob = serde_json::to_string(&creds).context("serializing IMAP credentials")?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: blob,
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        },
    )?;
    // Record the (non-secret) account on the cursor so status/index show it.
    let mut state = vault.read_imap_sync();
    state.account = Some(creds.username);
    vault.write_imap_sync(&state)
}

/// Forget the stored creds. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = creds are stored; the label is the account address.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(creds) = load_creds(vault)? {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: creds.username,
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // an app password doesn't carry an expiry we parse
            needs_reconnect: false,
            extra: BTreeMap::from([("host", creds.host)]),
        });
    }
    // No bring-your-own-app step: an app password is self-service.
    Ok(ConnectStatus {
        configured: true,
        accounts,
    })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. One method: paste
/// `email app-password [host [port]]`.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "imap",
    display_name: "IMAP Email",
    methods: &[ConnectMethod::TokenPaste {
        label: "Email + app password",
        help: "Paste your email then an app-specific password (space-separated): \
               `you@icloud.com abcdefghijklmnop`. Create the app password in your provider's \
               account-security settings — for iCloud, appleid.apple.com → Sign-In & Security → \
               App-Specific Passwords. The host is detected for common providers; for others, \
               append it: `you@example.com app-password mail.example.com`. Connect early — \
               server-deleted mail is gone before Trove can save it.",
        placeholder: "you@icloud.com app-password",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["imap"],
    setup: &[
        "Create an app-specific password in your mail provider's account-security settings (NOT your login password).",
        "Paste your email and that password here, space-separated.",
        "If Trove doesn't recognize your provider, append the IMAP host (and optionally a port): `you@example.com app-password mail.example.com 993`.",
        "Connect early — mail the server has already deleted can't be backfilled.",
    ],
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-imap-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- parse_connect_field ------------------------------------------------

    #[test]
    fn parse_two_tokens_derives_host_from_domain() {
        let c = parse_connect_field("me@icloud.com abcd-pass").unwrap();
        assert_eq!(c.username, "me@icloud.com");
        assert_eq!(c.password, "abcd-pass");
        assert_eq!(c.host, "imap.mail.me.com");
        assert_eq!(c.port, DEFAULT_PORT);
        // A few more providers from the table.
        assert_eq!(parse_connect_field("x@yahoo.com p").unwrap().host, "imap.mail.yahoo.com");
        assert_eq!(parse_connect_field("x@zoho.com p").unwrap().host, "imap.zoho.com");
        assert_eq!(parse_connect_field("x@fastmail.com p").unwrap().host, "imap.fastmail.com");
        // Domain match is case-insensitive.
        assert_eq!(parse_connect_field("X@ICloud.COM p").unwrap().host, "imap.mail.me.com");
    }

    #[test]
    fn parse_three_tokens_takes_explicit_host() {
        let c = parse_connect_field("me@example.com secret mail.example.com").unwrap();
        assert_eq!(c.host, "mail.example.com");
        assert_eq!(c.port, DEFAULT_PORT);
    }

    #[test]
    fn parse_four_tokens_takes_host_and_port() {
        let c = parse_connect_field("me@example.com secret mail.example.com 1993").unwrap();
        assert_eq!(c.host, "mail.example.com");
        assert_eq!(c.port, 1993);
    }

    #[test]
    fn parse_unknown_domain_without_host_errors() {
        let err = parse_connect_field("me@example.com secret").unwrap_err().to_string();
        assert!(err.contains("unknown provider"), "{err}");
        assert!(err.contains("mail.example.com"), "names the host placeholder: {err}");
    }

    #[test]
    fn parse_garbage_errors() {
        // No '@' in the first token.
        assert!(parse_connect_field("notanemail secret").is_err());
        // Missing password.
        assert!(parse_connect_field("me@icloud.com").is_err());
        // Empty.
        assert!(parse_connect_field("   ").is_err());
        // Bad port.
        assert!(parse_connect_field("me@example.com p mail.example.com notaport").is_err());
        // Too many tokens.
        assert!(parse_connect_field("me@example.com p host 993 extra").is_err());
    }

    // --- cursor logic -------------------------------------------------------

    #[test]
    fn plan_same_uidvalidity_is_incremental() {
        let stored = FolderCursor { uidvalidity: 7, max_uid: 42 };
        assert_eq!(plan_fetch(Some(&stored), 7), FetchPlan::After(42));
        assert_eq!(FetchPlan::After(42).uid_range(), "43:*");
    }

    #[test]
    fn plan_changed_uidvalidity_refetches_from_one() {
        let stored = FolderCursor { uidvalidity: 7, max_uid: 42 };
        assert_eq!(plan_fetch(Some(&stored), 99), FetchPlan::FromStart);
        assert_eq!(FetchPlan::FromStart.uid_range(), "1:*");
    }

    #[test]
    fn plan_new_folder_fetches_from_one() {
        assert_eq!(plan_fetch(None, 7), FetchPlan::FromStart);
        // A zero stored validity (degenerate) also restarts.
        let zero = FolderCursor { uidvalidity: 0, max_uid: 10 };
        assert_eq!(plan_fetch(Some(&zero), 0), FetchPlan::FromStart);
    }

    #[test]
    fn cursor_advances_to_new_max_uid() {
        let stored = FolderCursor { uidvalidity: 7, max_uid: 42 };
        // Same validity, fetched up to 50 → advance to 50.
        let c = advanced_cursor(Some(&stored), 7, Some(50));
        assert_eq!(c, FolderCursor { uidvalidity: 7, max_uid: 50 });
        // Same validity, nothing fetched → keep 42 (don't move backward).
        let c = advanced_cursor(Some(&stored), 7, None);
        assert_eq!(c.max_uid, 42);
        // Changed validity → max restarts from this pass's fetch.
        let c = advanced_cursor(Some(&stored), 99, Some(3));
        assert_eq!(c, FolderCursor { uidvalidity: 99, max_uid: 3 });
        // New folder → straight to the fetched max.
        let c = advanced_cursor(None, 5, Some(8));
        assert_eq!(c, FolderCursor { uidvalidity: 5, max_uid: 8 });
    }

    // --- canned RFC822 → Message → append/dedup -----------------------------

    const PLAIN: &str = "Message-ID: <plain@example.com>\r\n\
Date: Wed, 10 Jun 2026 09:00:00 -0700\r\n\
From: Alice Example <alice@example.com>\r\n\
To: me@icloud.com\r\n\
Subject: Plain hello\r\n\
\r\n\
Just a plain text body.\r\n";

    const MULTIPART: &str = "Message-ID: <multi@example.com>\r\n\
Date: Wed, 10 Jun 2026 10:00:00 -0700\r\n\
From: Bob <bob@example.com>\r\n\
To: me@icloud.com\r\n\
Subject: Multipart\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=\"BOUND\"\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
The plain part.\r\n\
--BOUND\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>The HTML part.</p>\r\n\
--BOUND--\r\n";

    const WITH_ATTACHMENT: &str = "Message-ID: <attach@example.com>\r\n\
Date: Wed, 10 Jun 2026 11:00:00 -0700\r\n\
From: Carol <carol@example.com>\r\n\
To: me@icloud.com\r\n\
Subject: Here's a file\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"MIX\"\r\n\
\r\n\
--MIX\r\n\
Content-Type: text/plain\r\n\
\r\n\
See attached.\r\n\
--MIX\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--MIX--\r\n";

    // No Message-ID — guid falls back to a content hash (stable for dedupe).
    const NO_ID: &str = "Date: Wed, 10 Jun 2026 12:00:00 -0700\r\n\
From: Dan <dan@example.com>\r\n\
To: me@icloud.com\r\n\
Subject: No message id\r\n\
\r\n\
Body without a Message-ID header.\r\n";

    fn bytes(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn canned_rfc822_writes_to_email_stream_with_service_and_folder_label() {
        let v = temp_vault("store");
        let mut seen = v.correspondence_guids(SOURCE).unwrap();
        let raws = vec![bytes(PLAIN), bytes(MULTIPART), bytes(WITH_ATTACHMENT), bytes(NO_ID)];
        let n = store_folder_messages(&v, "INBOX", "me@icloud.com", &raws, &mut seen).unwrap();
        assert_eq!(n, 4);

        // All four land in the shared email stream for June 2026.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 4);
        for m in &day {
            assert_eq!(m.source, "email");
            assert_eq!(m.service, "me@icloud.com");
            assert_eq!(m.labels, vec!["INBOX".to_string()], "folder is the label");
        }

        // The plain message parsed sender/subject via the shared parser.
        let plain = day.iter().find(|m| m.guid == "<plain@example.com>").unwrap();
        assert_eq!(plain.sender, "alice@example.com");
        assert_eq!(plain.subject, "Plain hello");
        assert!(plain.text.contains("plain text body"));

        // Multipart: text part preferred.
        let multi = day.iter().find(|m| m.guid == "<multi@example.com>").unwrap();
        assert!(multi.text.contains("The plain part"));

        // Attachment metadata captured (no file stored).
        let attach = day.iter().find(|m| m.guid == "<attach@example.com>").unwrap();
        assert_eq!(attach.attachments.len(), 1);
        assert_eq!(attach.attachments[0].name, "report.pdf");
        assert!(attach.attachments[0].mime.contains("pdf"));

        // No-Message-ID message got a content-hash guid.
        let no_id = day.iter().find(|m| m.guid.starts_with("sha256:")).unwrap();
        assert_eq!(no_id.subject, "No message id");
    }

    #[test]
    fn duplicate_guid_is_skipped_across_reaches() {
        // Simulate the same message reached twice (e.g. via IMAP and already in
        // Gmail's folder, or two overlapping IMAP passes): the shared `email`
        // dedupe set must skip the second.
        let v = temp_vault("dedup");

        // First, pretend Gmail already wrote this exact message to the stream.
        let mut existing = crate::email::email_to_message(PLAIN.as_bytes(), "me@icloud.com").unwrap();
        existing.labels = vec!["INBOX".into()];
        v.append_messages(&[existing]).unwrap();

        let mut seen = v.correspondence_guids(SOURCE).unwrap();
        assert!(seen.contains("<plain@example.com>"));

        // Now the IMAP fetch reaches the same message plus a new one.
        let raws = vec![bytes(PLAIN), bytes(MULTIPART)];
        let n = store_folder_messages(&v, "INBOX", "me@icloud.com", &raws, &mut seen).unwrap();
        assert_eq!(n, 1, "the already-stored guid is skipped, only the new one written");

        // The stream holds exactly two distinct messages, not three.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 2);

        // A duplicate WITHIN one batch is also caught.
        let mut seen2 = v.correspondence_guids(SOURCE).unwrap();
        let dup_batch = vec![bytes(NO_ID), bytes(NO_ID)];
        let n2 = store_folder_messages(&v, "Archive", "me@icloud.com", &dup_batch, &mut seen2).unwrap();
        assert_eq!(n2, 1, "the second copy in the same batch is deduped");
    }

    // --- pull_with over a stub (no network) ---------------------------------

    /// A canned IMAP server: two folders, fixed UIDVALIDITY + bodies per UID.
    struct StubOps {
        folders: Vec<String>,
        uidvalidity: BTreeMap<String, u32>,
        bodies: BTreeMap<String, BTreeMap<u32, Vec<u8>>>,
        selected: Option<String>,
    }

    impl ImapOps for StubOps {
        fn folders(&mut self) -> Result<Vec<String>> {
            Ok(self.folders.clone())
        }
        fn examine(&mut self, folder: &str) -> Result<FolderState> {
            self.selected = Some(folder.to_string());
            Ok(FolderState {
                name: folder.to_string(),
                uidvalidity: *self.uidvalidity.get(folder).unwrap_or(&0),
            })
        }
        fn search_uids(&mut self, _range: &str) -> Result<Vec<u32>> {
            let f = self.selected.as_deref().unwrap_or("");
            let mut uids: Vec<u32> = self
                .bodies
                .get(f)
                .map(|m| m.keys().copied().collect())
                .unwrap_or_default();
            uids.sort_unstable();
            Ok(uids)
        }
        fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<Vec<u8>>> {
            let f = self.selected.as_deref().unwrap_or("");
            let folder = self.bodies.get(f);
            Ok(uids
                .iter()
                .filter_map(|u| folder.and_then(|m| m.get(u)).cloned())
                .collect())
        }
    }

    #[test]
    fn pull_with_writes_folders_advances_cursors_and_dedupes_on_resync() {
        let v = temp_vault("pull");
        let mut ops = StubOps {
            folders: vec!["INBOX".into(), "Archive".into()],
            uidvalidity: BTreeMap::from([("INBOX".into(), 10), ("Archive".into(), 20)]),
            bodies: BTreeMap::from([
                ("INBOX".to_string(), BTreeMap::from([(1u32, bytes(PLAIN)), (2u32, bytes(MULTIPART))])),
                ("Archive".to_string(), BTreeMap::from([(5u32, bytes(WITH_ATTACHMENT))])),
            ]),
            selected: None,
        };

        let n = pull_with(&v, &mut ops, "me@icloud.com").unwrap();
        assert_eq!(n, 3, "three messages across two folders");

        // Cursors advanced per folder to the max UID under the server validity.
        let state = v.read_imap_sync();
        assert_eq!(state.account.as_deref(), Some("me@icloud.com"));
        assert_eq!(state.folders["INBOX"], FolderCursor { uidvalidity: 10, max_uid: 2 });
        assert_eq!(state.folders["Archive"], FolderCursor { uidvalidity: 20, max_uid: 5 });
        assert_eq!(state.messages, 3);

        // The messages are in the shared email stream with folder labels.
        let day = v.correspondence_timeline("2026-06-10").unwrap();
        assert_eq!(day.len(), 3);
        let inbox_labels: Vec<_> = day.iter().filter(|m| m.labels == vec!["INBOX".to_string()]).collect();
        assert_eq!(inbox_labels.len(), 2);

        // Re-sync with the same server state: nothing new (guid dedupe +
        // incremental cursor), and the stream is unchanged.
        let again = pull_with(&v, &mut ops, "me@icloud.com").unwrap();
        assert_eq!(again, 0, "idempotent re-sync");
        assert_eq!(v.correspondence_timeline("2026-06-10").unwrap().len(), 3);
        assert_eq!(v.read_imap_sync().messages, 3);
    }

    // --- secret handling ----------------------------------------------------

    #[test]
    fn connect_stores_creds_as_0600_secret_password_absent_from_cursor() {
        // Can't do a live LOGIN in tests, so write the secret directly the way
        // def_connect would (its only extra step is the live validation) and
        // assert the storage invariants.
        let v = temp_vault("secret");
        let creds = ImapCreds {
            host: "imap.mail.me.com".into(),
            port: 993,
            username: "me@icloud.com".into(),
            password: "super-secret-app-pw".into(),
        };
        let blob = serde_json::to_string(&creds).unwrap();
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: blob,
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let mut state = v.read_imap_sync();
        state.account = Some(creds.username.clone());
        v.write_imap_sync(&state).unwrap();

        // Round-trips out of the secret store.
        let loaded = load_creds(&v).unwrap().unwrap();
        assert_eq!(loaded, creds);

        // The secret file is 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = v.root().join(".trove/sync/imap-token.json");
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "creds file must be 0600");
        }

        // The password is NOT in the non-secret cursor file.
        let cursor = fs::read_to_string(v.root().join(SYNC_FILE)).unwrap();
        assert!(!cursor.contains("super-secret-app-pw"), "password leaked into cursor: {cursor}");
        assert!(!cursor.contains("password"), "no password field in cursor: {cursor}");
        // The account address (non-secret) is fine to record.
        assert!(cursor.contains("me@icloud.com"));

        // disconnect forgets the secret.
        def_disconnect(&v, SERVICE).unwrap();
        assert!(load_creds(&v).unwrap().is_none());
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn status_reflects_stored_creds() {
        let v = temp_vault("status");
        assert!(def_status(&v).unwrap().accounts.is_empty());
        let creds = ImapCreds {
            host: "imap.fastmail.com".into(),
            port: 993,
            username: "me@fastmail.com".into(),
            password: "pw".into(),
        };
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: serde_json::to_string(&creds).unwrap(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "me@fastmail.com");
        assert_eq!(status.accounts[0].extra.get("host"), Some(&"imap.fastmail.com".to_string()));
        assert!(!status.accounts[0].needs_reconnect);
    }

    #[test]
    fn connection_exposes_token_paste() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "imap");
    }
}
