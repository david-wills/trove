//! troved — the headless always-on Trove collector.
//!
//! A thin shell around `trove_core::run_watcher`: opens the vault and contends
//! for the same single-writer lock the GUI app uses, so exactly one process
//! collects at a time and handoff in either direction is automatic. Designed
//! to run 24/7 under launchd as a user agent.
//!
//! Commands:
//!   troved run         collect until SIGTERM/SIGINT (launchd mode; default)
//!   troved install     write + bootstrap the launch agent (starts at login)
//!                      and the Chrome native messaging host manifest
//!   troved uninstall   stop and remove the launch agent
//!   troved status      installed? running? who is collecting right now?
//!
//! Chrome also spawns this binary as the browser extension's native
//! messaging host, passing the extension origin as the first argument
//! (manifests can't carry custom args) — `main` dispatches that to
//! [`native_host`].
//!
//! `run` accepts `--vault <path>` (or TROVE_VAULT) to target a non-default
//! vault — used by tests/dev so smoke runs never touch the real vault.

mod native_host;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use trove_core::{
    daemon_plist_path, native_host_manifest_path, run_watcher, Vault, WatchControl, WatcherRole,
    DAEMON_LABEL, EXTENSION_ID, NATIVE_HOST_NAME,
};

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let result = match cmd {
        origin if origin.starts_with("chrome-extension://") => native_host::run(origin),
        "run" => run(&args[1..]),
        "install" => install(),
        "uninstall" => uninstall(),
        "status" => status(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("troved: unknown command `{other}`\n");
            print_help();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("troved: {e:#}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "troved — headless Trove collector daemon\n\n\
         usage: troved [run|install|uninstall|status]\n\n\
         run [--vault <path>]   collect activity until SIGTERM/SIGINT (default)\n\
         install                write the launch agent plist and start it (runs at login);\n\
                                also installs the Chrome native messaging host manifest\n\
         uninstall              stop the agent and remove the plist + host manifest\n\
         status                 show install/running/collector state"
    );
}

/// The vault root for `run`: --vault flag > TROVE_VAULT env > ~/Documents/Trove.
fn vault_root(args: &[String]) -> Result<PathBuf> {
    if let Some(i) = args.iter().position(|a| a == "--vault") {
        let path = args.get(i + 1).context("--vault requires a path")?;
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("TROVE_VAULT") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    Ok(Vault::default_root())
}

fn run(args: &[String]) -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    let root = vault_root(args)?;
    println!(
        "troved: watching vault at {} (pid {})",
        root.display(),
        std::process::id()
    );
    if !trove_core::screen_recording_ok() {
        println!(
            "troved: Screen Recording not granted for this binary — other apps' window \
             titles will be empty (app-level tracking works regardless)"
        );
    }

    let control = WatchControl::new();
    // Bridge the async-signal-safe flag into a controlled stop (the handler
    // itself can only touch the static atomic).
    let bridge = control.clone();
    std::thread::spawn(move || loop {
        if STOP.load(Ordering::SeqCst) {
            bridge.stop();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });

    // The watcher loop runs on a worker thread; the MAIN thread must pump
    // the CF run loop instead, because distributed notifications (the Music
    // scrobbler's source) are only delivered on the process's main run loop.
    let worker = {
        let control = control.clone();
        std::thread::Builder::new()
            .name("trove-watcher".into())
            .spawn(move || run_watcher(root, WatcherRole::Daemon, control))
            .context("spawning watcher thread")?
    };
    trove_core::pump_main_run_loop(|| control.stopped());
    match worker.join() {
        Ok(result) => result?,
        Err(_) => bail!("watcher thread panicked"),
    }
    println!("troved: stopped cleanly (open event flushed)");
    Ok(())
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

fn plist_path() -> Result<PathBuf> {
    daemon_plist_path().context("no home directory")
}

/// True if launchd currently has our service bootstrapped in the gui domain.
fn service_loaded() -> bool {
    Command::new("launchctl")
        .args(["print", &format!("gui/{}/{}", uid(), DAEMON_LABEL)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// (Re)load the launch agent from `plist`, race-free.
///
/// `launchctl bootout` returns *before* teardown finishes, so an immediate
/// `bootstrap` races the still-registered service and fails with the infamous
/// `5: Input/output error`, leaving nothing loaded. We bootout, poll until the
/// service is actually gone, then bootstrap with a short retry to absorb any
/// residual transient I/O errors.
fn restart_service(plist: &Path) -> Result<()> {
    let domain = format!("gui/{}", uid());
    let target = format!("{}/{}", domain, DAEMON_LABEL);

    let _ = Command::new("launchctl").args(["bootout", &target]).output();

    // Wait (up to ~5s) for launchd to finish removing the old service.
    for _ in 0..50 {
        if !service_loaded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let mut last_err = String::new();
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(plist)
            .output()
            .context("running launchctl bootstrap")?;
        if out.status.success() {
            return Ok(());
        }
        // If a stale copy is somehow still loaded, a kickstart restarts it in
        // place onto the freshly written plist — good enough for a rebuild.
        if service_loaded() {
            let _ = Command::new("launchctl")
                .args(["kickstart", "-k", &target])
                .output();
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    bail!("launchctl bootstrap failed after retries: {last_err}");
}

fn install() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd management is macOS-only");
    }
    let exe = std::env::current_exe()
        .context("locating troved binary")?
        .canonicalize()
        .context("canonicalizing troved path")?;
    let logs = dirs::home_dir()
        .context("no home directory")?
        .join("Library/Logs/trove");
    std::fs::create_dir_all(&logs)?;

    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>StandardOutPath</key><string>{logs}/troved.log</string>
    <key>StandardErrorPath</key><string>{logs}/troved.err.log</string>
</dict>
</plist>
"#,
        label = DAEMON_LABEL,
        exe = exe.display(),
        logs = logs.display(),
    );
    std::fs::write(&plist, body).with_context(|| format!("writing {}", plist.display()))?;

    // Restart-safe: tear down any loaded copy, wait for it to clear, then
    // bootstrap fresh — see restart_service for the race this avoids.
    restart_service(&plist)?;
    println!("troved: installed and started ({})", exe.display());
    println!("troved: logs at {}/troved.log", logs.display());
    println!("troved: note — after rebuilding the binary, run `troved install` again to restart on the new build");

    let manifest = install_native_host_manifest(&exe)?;
    println!(
        "troved: Chrome native messaging host manifest at {}",
        manifest.display()
    );
    Ok(())
}

/// Register this binary as the browser extension's native messaging host.
/// Chrome reads the manifest at connect time, so reinstalls (new binary
/// path) take effect on the extension's next reconnect — no restart needed.
fn install_native_host_manifest(exe: &Path) -> Result<PathBuf> {
    let path = native_host_manifest_path().context("no home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "name": NATIVE_HOST_NAME,
        "description": "Trove browser watcher host (writes tab spans into the local vault)",
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&body)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd management is macOS-only");
    }
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid(), DAEMON_LABEL)])
        .output();
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("troved: stopped and removed {}", plist.display());
    } else {
        println!("troved: not installed (no plist at {})", plist.display());
    }
    if let Some(manifest) = native_host_manifest_path() {
        if manifest.exists() {
            std::fs::remove_file(&manifest)
                .with_context(|| format!("removing {}", manifest.display()))?;
            println!("troved: removed native messaging host manifest");
        }
    }
    Ok(())
}

fn status() -> Result<()> {
    let plist = plist_path()?;
    println!(
        "launch agent: {}",
        if plist.exists() {
            format!("installed ({})", plist.display())
        } else {
            "not installed".into()
        }
    );

    if cfg!(target_os = "macos") {
        println!(
            "launchd service: {}",
            if service_loaded() { "loaded" } else { "not loaded" }
        );
    }

    println!(
        "browser extension host: {}",
        match native_host_manifest_path() {
            Some(m) if m.exists() => format!("manifest installed ({})", m.display()),
            _ => "manifest not installed (run `troved install`)".into(),
        }
    );

    let vault = Vault::open_or_create(Vault::default_root())?;
    match vault.read_watcher_state() {
        Some(s) if s.is_fresh() => {
            let doing = s
                .current
                .as_ref()
                .map(|c| {
                    if c.afk {
                        "away".to_string()
                    } else {
                        format!("in {}", c.app)
                    }
                })
                .unwrap_or_else(|| "idle".into());
            println!("collector: {} (pid {}), currently {}", s.role, s.pid, doing);
        }
        Some(_) => println!("collector: none (stale heartbeat — last owner likely crashed)"),
        None => println!("collector: none"),
    }
    Ok(())
}
