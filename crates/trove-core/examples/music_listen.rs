//! Debug harness: print every decoded Music playerInfo event for 30s.
//! Run with: cargo run -p trove-core --example music_listen
//!
//! Demonstrates the host contract: events only flow while the process's main
//! run loop is pumping (see `music_listener` module docs).

use std::time::{Duration, Instant};

use trove_core::{pump_main_run_loop, MusicListener};

fn main() {
    let (listener, rx) = MusicListener::start();
    std::thread::spawn(move || {
        for (ts, ev) in rx {
            println!("{} {:?}", ts.format("%H:%M:%S"), ev);
        }
    });
    println!("listening for Music playerInfo events for 30s…");
    let end = Instant::now() + Duration::from_secs(30);
    pump_main_run_loop(|| Instant::now() >= end);
    listener.stop();
    println!("done");
}
