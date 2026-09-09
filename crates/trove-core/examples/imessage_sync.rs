//! Debug harness: run one iMessage sync pass into a throwaway vault and
//! print what happened. Never touches the real ~/Trove.
//!
//! ```bash
//! cargo run -p trove-core --example imessage_sync -- /tmp/trove-imessage-validate
//! ```

use trove_core::Vault;

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .expect("usage: imessage_sync <temp-vault-dir>");
    assert!(
        !root.contains("/Trove"),
        "refusing to run against what looks like the real vault"
    );
    let vault = Vault::open_or_create(root.into())?;
    let t0 = std::time::Instant::now();
    let stats = vault.collect_imessages()?;
    println!(
        "available={} new_messages={} elapsed={:.2?}",
        stats.available,
        stats.new_messages,
        t0.elapsed()
    );
    let state = vault.read_imessage_sync();
    println!("cursor: {:?}", state.map(|s| s.cursor));

    // Second pass must be a no-op.
    let t1 = std::time::Instant::now();
    let again = vault.collect_imessages()?;
    println!(
        "second pass: new_messages={} elapsed={:.2?}",
        again.new_messages,
        t1.elapsed()
    );

    // Exercise the read side the UI uses.
    let t2 = std::time::Instant::now();
    let s = vault.correspondence_summary("2026-05-12", "2026-06-10")?;
    println!(
        "30d summary: messages={} sent={} received={} chats={} elapsed={:.2?}",
        s.messages,
        s.sent,
        s.received,
        s.chats.len(),
        t2.elapsed()
    );
    let daily = vault.correspondence_daily("2026-05-12", "2026-06-10")?;
    println!("30d daily points: {}", daily.len());
    Ok(())
}
