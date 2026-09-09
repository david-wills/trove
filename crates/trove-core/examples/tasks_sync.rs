//! Debug harness: run one TickTick sync pass against a vault and print the
//! stats. Defaults to a temp vault path so it can't touch ~/Trove by
//! accident; point it at a copy of the real tasks/ + token to validate
//! end-to-end.
//!
//!     cargo run -p trove-core --example tasks_sync -- /tmp/trove-tasks-validate

use trove_core::Vault;

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("trove-tasks-sync-example"));
    println!("vault: {}", root.display());
    let vault = Vault::open_or_create(root)?;
    let start = std::time::Instant::now();
    let stats = vault.collect_tasks()?;
    println!(
        "synced in {:?}: {} projects, {} open, {} completed, {} created, {} deleted",
        start.elapsed(),
        stats.projects,
        stats.open,
        stats.completed,
        stats.created,
        stats.deleted
    );
    Ok(())
}
