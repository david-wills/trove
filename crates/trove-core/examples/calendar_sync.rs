//! Debug harness: run one Calendar + Reminders sync pass into a throwaway
//! vault and print what happened. Never touches the real ~/Documents/Trove.
//!
//! ```bash
//! cargo run --release -p trove-core --example calendar_sync -- /tmp/trove-calendar-validate
//! ```

use trove_core::Vault;

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .expect("usage: calendar_sync <temp-vault-dir>");
    assert!(
        !root.contains("/Trove"),
        "refusing to run against what looks like the real vault"
    );
    let vault = Vault::open_or_create(root.into())?;

    println!(
        "auth — events: {}, reminders: {}",
        trove_core::events_auth_status().as_str(),
        trove_core::reminders_auth_status().as_str()
    );

    let t0 = std::time::Instant::now();
    let stats = vault.collect_calendar()?;
    println!("calendar pass 1 ({:?}): {stats:?}", t0.elapsed());
    if let Some(state) = vault.read_calendar_sync() {
        println!("sync state: {state:?}");
    }

    let t1 = std::time::Instant::now();
    let stats = vault.collect_calendar()?;
    println!("calendar pass 2 ({:?}): {stats:?}", t1.elapsed());

    let t2 = std::time::Instant::now();
    let stats = vault.collect_reminders()?;
    println!("reminders pass ({:?}): {stats:?}", t2.elapsed());

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let week_ago = (chrono::Local::now() - chrono::Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    let summary = vault.calendar_summary(&week_ago, &today)?;
    println!(
        "last 7d: {} events, {:.1}h scheduled, {} calendars",
        summary.events,
        summary.hours,
        summary.calendars.len()
    );
    for occ in vault.calendar_timeline(&today)?.iter().take(8) {
        println!(
            "  today: {} | {} | {}{}",
            &occ.start[11..16.min(occ.start.len())],
            occ.title,
            occ.calendar,
            if occ.all_day { " (all day)" } else { "" }
        );
    }
    Ok(())
}
