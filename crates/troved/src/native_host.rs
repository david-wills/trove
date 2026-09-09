//! Native messaging host mode — the receiving end of the Trove browser
//! watcher extension.
//!
//! Chrome spawns this same `troved` binary (manifest written by `troved
//! install`) whenever the extension connects, passing the extension origin
//! as the first argument — native messaging manifests cannot carry custom
//! args, so `main` dispatches here on the `chrome-extension://` prefix. One
//! host process runs per Chrome profile with the extension; it is *not* the
//! launchd daemon and does not contend for the watcher lock — extension
//! spans are appended directly through trove-core, whose per-day-file flock
//! keeps concurrent writers (other profiles' hosts, the daemon's history
//! sync) from interleaving lines.
//!
//! Protocol: Chrome native messaging — a u32 native-endian length prefix,
//! then that many bytes of JSON, repeated; EOF (browser quit, extension
//! disabled, service worker suspended) means disconnect. Messages are tagged
//! by `type`: `snapshot` feeds the [`TabTracker`] state machine and appends
//! whatever spans closed; `ads` (the opt-in page observer's batches) appends
//! directly — viewability accrual happened browser-side, the host only
//! stamps arrival time as a sanity bound. Unknown types are logged and
//! skipped, so old hosts tolerate new extensions and vice versa. Nothing is
//! ever written to stdout (it belongs to the protocol) — logs go to stderr,
//! which Chrome surfaces in its own log.

use std::io::{self, Read};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::Deserialize;
use trove_core::{AdEvent, BrowserVisit, ExtConfig, ExtSnapshot, TabTracker, Vault};

/// When the machine has been idle this long, a focused browser window no
/// longer means engagement — the extension can't see system idle, but the
/// host can (same threshold as the activity watcher's AFK default). Audible
/// playback still counts: media keeps playing to an idle room on purpose.
const IDLE_GATE_SECS: f64 = 120.0;

/// Snapshots are a few hundred bytes; anything near this is corruption.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Everything the extension sends, dispatched on the `type` field.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostMessage {
    Snapshot(ExtSnapshot),
    Ads { events: Vec<AdEvent> },
}

pub fn run(origin: &str) -> Result<()> {
    let root = match std::env::var("TROVE_VAULT") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => Vault::default_root(),
    };
    let vault = Vault::open_or_create(root)?;
    let mut tracker = TabTracker::new("chrome", ExtConfig::default());
    // One sidecar per host process (Chrome runs one host per profile), keyed by
    // pid so concurrent profiles don't clobber each other's live state.
    let live_key = std::process::id().to_string();
    eprintln!("troved native-host: connected (origin {origin}, pid {live_key})");

    let mut stdin = io::stdin().lock();
    serve(&vault, &mut tracker, &live_key, &mut stdin)?;

    let last = tracker.flush();
    append(&vault, &last);
    // No open spans remain — drop the sidecar so the UI stops showing this
    // host's rows immediately instead of waiting for the staleness TTL.
    let _ = vault.clear_browser_live(&live_key);
    eprintln!("troved native-host: disconnected, open spans flushed");
    Ok(())
}

/// The message loop, split from [`run`] so tests can pipe framed messages at
/// a temp vault. Returns on clean EOF; open spans are the caller's to flush.
fn serve(
    vault: &Vault,
    tracker: &mut TabTracker,
    live_key: &str,
    input: &mut impl Read,
) -> Result<()> {
    while let Some(frame) = read_frame(input)? {
        let msg: HostMessage = match serde_json::from_slice(&frame) {
            Ok(m) => m,
            Err(e) => {
                // Unknown `type` lands here too — forward compatibility.
                eprintln!("troved native-host: ignoring unparseable message: {e}");
                continue;
            }
        };
        match msg {
            HostMessage::Snapshot(mut snap) => {
                if !vault.integration_enabled("browser-extension") {
                    // Hub toggle is off: close out anything collected while
                    // it was on, then idle. The connection stays up, so
                    // re-enabling resumes within a snapshot.
                    append(vault, &tracker.flush());
                    let _ = vault.clear_browser_live(live_key);
                    continue;
                }
                if snap.focused && trove_core::sample().idle_seconds >= IDLE_GATE_SECS {
                    snap.focused = false;
                }
                let now = Local::now();
                let closed = tracker.tick(now, &snap);
                append(vault, &closed);
                // Publish what's still open so the UI can show "watching now"
                // live, without waiting for the span to close. Best-effort: a
                // failed write just means the live view lags a snapshot.
                if let Err(e) = vault.write_browser_live("chrome", live_key, &tracker.live(now)) {
                    eprintln!("troved native-host: failed to write live state: {e:#}");
                }
            }
            HostMessage::Ads { events } => {
                // Defense in depth: gated on its own hub flag, separate from
                // "browser-extension" — even a confused extension can't write
                // a stream the vault owner turned off.
                if events.is_empty() || !vault.integration_enabled("browser-ads") {
                    continue;
                }
                // The opt-in identity resolver is the lone networked path:
                // when on, it fetches Google's transparency page per ad to
                // name the payer. Off by default → no network, same as before.
                let resolve = vault.integration_enabled("browser-ads-identify");
                if let Err(e) = vault.ingest_ad_events(&events, Local::now(), resolve) {
                    eprintln!("troved native-host: failed to append ad events: {e:#}");
                }
            }
        }
    }
    Ok(())
}

/// Append failures are logged, never fatal — losing one batch beats killing
/// the connection and losing the open spans with it.
fn append(vault: &Vault, rows: &[BrowserVisit]) {
    if rows.is_empty() {
        return;
    }
    if let Err(e) = vault.append_browser_visits(rows) {
        eprintln!("troved native-host: failed to append spans: {e:#}");
    }
}

/// Read one frame; `None` on clean EOF at a frame boundary.
fn read_frame(r: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    if let Err(e) = r.read_exact(&mut len) {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(e).context("reading frame length");
    }
    let n = u32::from_ne_bytes(len) as usize;
    if n > MAX_FRAME_BYTES {
        bail!("oversized native messaging frame ({n} bytes)");
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).context("reading frame body")?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("troved-host-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn frame(json: &str) -> Vec<u8> {
        let mut out = (json.len() as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(json.as_bytes());
        out
    }

    fn ads_json(end_ms: i64) -> String {
        format!(
            r#"{{"type":"ads","events":[{{"ts_ms":{},"end_ms":{end_ms},
                "page_url":"https://example.com/","frame_url":"https://googleads.g.doubleclick.net/x",
                "viewed_secs":3.5,"viewable":true,"w":300,"h":250}}]}}"#,
            end_ms - 60_000
        )
    }

    /// Mixed snapshot / ads / unknown / garbage frames on one connection:
    /// each type dispatches to its own path and bad frames never kill the
    /// loop (extends the session-6 smoke-test pattern against a temp vault).
    #[test]
    fn serve_dispatches_mixed_messages() {
        let v = temp_vault("dispatch");
        let mut tracker = TabTracker::new("chrome", ExtConfig::default());
        let end_ms = Local::now().timestamp_millis() - 5_000;
        let mut wire = Vec::new();
        wire.extend(frame(r#"{"type":"snapshot","focused":false,"tab_count":3,"active":null,"audible":[]}"#));
        wire.extend(frame(&ads_json(end_ms)));
        wire.extend(frame(r#"{"type":"telepathy","payload":1}"#)); // future message type
        wire.extend(frame(r#"not json at all"#));
        wire.extend(frame(&ads_json(end_ms + 1_000)));
        serve(&v, &mut tracker, "test", &mut io::Cursor::new(wire)).unwrap();

        let day = Local::now().format("%Y-%m-%d").to_string();
        let ads = v.ads_timeline(&day).unwrap();
        assert_eq!(ads.len(), 2, "both ads batches landed despite junk between them");
        assert_eq!(ads[0].network, "doubleclick.net");
        assert!(ads[0].viewable);
        // The snapshot path ran too: the live sidecar was published.
        assert!(v.root().join(".trove/live").exists());
    }

    /// The browser-ads hub flag gates appends independently of the snapshot
    /// stream — defense in depth against a confused extension.
    #[test]
    fn ads_respect_their_own_hub_gate() {
        let v = temp_vault("gate");
        v.set_integration_enabled("browser-ads", false).unwrap();
        let mut tracker = TabTracker::new("chrome", ExtConfig::default());
        let wire = frame(&ads_json(Local::now().timestamp_millis()));
        serve(&v, &mut tracker, "test", &mut io::Cursor::new(wire)).unwrap();

        let day = Local::now().format("%Y-%m-%d").to_string();
        assert!(v.ads_timeline(&day).unwrap().is_empty());
        assert!(!v.root().join("browser/ads").exists());
    }

    #[test]
    fn frame_round_trip_and_eof() {
        let body = br#"{"focused":true}"#;
        let mut wire = (body.len() as u32).to_ne_bytes().to_vec();
        wire.extend_from_slice(body);
        let mut r = io::Cursor::new(wire);
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(&body[..]));
        assert!(read_frame(&mut r).unwrap().is_none(), "clean EOF");
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut r = io::Cursor::new((u32::MAX).to_ne_bytes().to_vec());
        assert!(read_frame(&mut r).is_err());
    }
}
