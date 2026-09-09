//! Debug harness: run one Screen Time (Biome) sync pass into a throwaway
//! vault and print what happened. Never touches the real ~/Trove. Reads the
//! real Biome streams, so the binary needs Full Disk Access (a terminal
//! with FDA inherits it).
//!
//! ```bash
//! cargo run -p trove-core --example screen_time_sync -- /tmp/trove-st-validate
//! ```

use trove_core::Vault;

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .expect("usage: screen_time_sync <temp-vault-dir>");
    assert!(
        !root.contains("/Trove"),
        "refusing to run against what looks like the real vault"
    );
    let vault = Vault::open_or_create(root.into())?;
    println!(
        "permission_ok={} mtime={:?}",
        trove_core::screen_time_permission_ok(),
        trove_core::screen_time_mtime()
    );

    let t0 = std::time::Instant::now();
    let stats = vault.collect_screen_time()?;
    println!(
        "devices={} new_sessions={} new_plays={} skipped_records={} elapsed={:.2?}",
        stats.devices,
        stats.new_sessions,
        stats.new_plays,
        stats.skipped_records,
        t0.elapsed()
    );
    for (uuid, info) in vault.screen_time_devices() {
        println!(
            "  {uuid} -> kind={} label={} platform={:?} model={} last_seen={}",
            info.kind, info.label, info.platform, info.model, info.last_seen
        );
    }

    // Second pass must be a no-op (mtime cache + cursors).
    let t1 = std::time::Instant::now();
    let again = vault.collect_screen_time()?;
    println!(
        "second pass: devices={} new_sessions={} elapsed={:.2?}",
        again.devices,
        again.new_sessions,
        t1.elapsed()
    );

    // Exercise the read side the UI will use.
    let t2 = std::time::Instant::now();
    let s = vault.screen_time_summary("2026-05-01", "2026-06-11", None, true)?;
    println!(
        "range summary: total_hours={:.1} apps={} devices={} elapsed={:.2?}",
        s.total_seconds as f64 / 3600.0,
        s.apps.len(),
        s.devices.len(),
        t2.elapsed()
    );
    for a in s.apps.iter().take(10) {
        println!("  {:>7.1}h  {} ({})", a.seconds as f64 / 3600.0, a.app, a.bundle_id);
    }
    for d in &s.devices {
        println!("  device {:>7.1}h  {} [{}]", d.seconds as f64 / 3600.0, d.label, d.kind);
    }
    let daily = vault.screen_time_daily("2026-05-01", "2026-06-11", None, true)?;
    println!("daily points: {}", daily.len());

    // The Now Playing arm feeding the unified media stream.
    let m = vault.media_summary("2026-05-01", "2026-06-11")?;
    println!(
        "media summary: plays={} partials={} sources={:?} devices={:?}",
        m.plays, m.partials, m.sources, m.devices
    );
    // The Now Playing arm is the iPhone/iPad device rows.
    for u in m
        .top
        .iter()
        .filter(|u| u.device.starts_with("iPhone") || u.device.starts_with("iPad"))
        .take(8)
    {
        println!(
            "  np {:>6.1}h  {} [{}/{}] plays={}",
            u.seconds as f64 / 3600.0,
            u.name,
            u.category,
            u.device,
            u.plays
        );
    }
    Ok(())
}
