//! Run one weather collector pass against a throwaway vault.
//!
//!     cargo run -p trove-core --example weather_sync -- /tmp/some-temp-vault [lat lon [place]]
//!
//! With lat/lon args a manual location is set first (the no-permission
//! path); without them the pass exercises the full ladder, CoreLocation
//! included. Refuses real-vault paths — this is a plumbing probe.

use trove_core::{Vault, WeatherLocation};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(root) = args.first() else {
        anyhow::bail!("usage: weather_sync <temp-vault-path> [lat lon [place]]");
    };
    if root.contains("/Trove") {
        anyhow::bail!("refusing to touch a real vault; pass a temp path");
    }
    let vault = Vault::open_or_create(root.into())?;
    if let (Some(lat), Some(lon)) = (args.get(1), args.get(2)) {
        vault.set_weather_location(Some(WeatherLocation {
            lat: lat.parse()?,
            lon: lon.parse()?,
            place: args.get(3).cloned().unwrap_or_default(),
        }))?;
    }
    let stats = vault.collect_weather()?;
    println!("pass 1: observed={} skipped={:?}", stats.observed, stats.skipped);
    let stats = vault.collect_weather()?;
    println!("pass 2: observed={} skipped={:?}", stats.observed, stats.skipped);
    println!("latest: {:#?}", vault.weather_latest()?);
    println!("state:  {:?}", vault.read_weather_sync());
    Ok(())
}
