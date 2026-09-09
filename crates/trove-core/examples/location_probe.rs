//! Probe the CoreLocation bridge: print auth status, request access if
//! undetermined, and try for a fix. Coordinates are printed rounded — this
//! is a plumbing check, not a data collector.
//!
//!     cargo run -p trove-core --example location_probe

fn main() {
    let status = trove_core::corelocation::auth_status();
    println!("auth status: {}", status.as_str());
    if status == trove_core::corelocation::AuthStatus::NotDetermined {
        println!("requesting access (10s wait)…");
        let granted = trove_core::corelocation::request_access(10);
        println!("request resolved: granted={granted}");
    }
    match trove_core::corelocation::current_location(10) {
        Some(fix) => println!(
            "fix: ~({:.2}, {:.2}) age={}s",
            fix.lat, fix.lon, fix.age_secs
        ),
        None => println!("no fix (denied, undetermined, or timed out)"),
    }
}
