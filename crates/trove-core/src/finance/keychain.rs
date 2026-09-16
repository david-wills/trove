//! macOS Keychain storage for the SimpleFIN access URL.
//!
//! The access URL is a bearer credential, so it lives in the Keychain —
//! never in the vault (the TickTick token's 0600-file approach is the
//! floor; finance credentials get the stronger home the plan calls for).
//!
//! Implemented by shelling out to `/usr/bin/security` rather than the
//! Security framework on purpose: keychain ACLs are per-binary, and the app
//! and a dev build are different (and frequently rebuilt) binaries. An
//! item created through `security` is readable through `security` from both
//! processes with no per-binary grant or prompt — and the daemon has no UI
//! session to answer a prompt with. `security` ships with macOS, so this
//! adds no external dependency. Writes go through `security -i` (commands on
//! stdin) so the secret never appears in any process's argv.

use anyhow::Result;

#[cfg(target_os = "macos")]
const SERVICE: &str = "trove-simplefin";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "access-url";

#[cfg(target_os = "macos")]
pub fn store_access_url(url: &str) -> Result<()> {
    use anyhow::{bail, Context};
    use std::io::Write;
    use std::process::{Command, Stdio};

    // The URL is interpolated into a quoted `security` command line; keep
    // out anything that could break the quoting (real access URLs are plain
    // https URLs, so this never fires in practice).
    if url.chars().any(|c| c.is_ascii_control() || c == '"' || c == '\\') {
        bail!("access URL contains characters that can't be stored safely");
    }
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("launching /usr/bin/security")?;
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(
            // -U updates in place when the item already exists (reconnect).
            format!("add-generic-password -U -s \"{SERVICE}\" -a \"{ACCOUNT}\" -w \"{url}\"\n")
                .as_bytes(),
        )
        .context("writing to security")?;
    let out = child.wait_with_output().context("waiting for security")?;
    if !out.status.success() {
        bail!(
            "storing the credential in the Keychain failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The stored access URL, or `None` when not connected (the only way the
/// lookup fails in practice — a locked keychain also lands here, and the
/// sync just no-ops until it's unlocked).
#[cfg(target_os = "macos")]
pub fn load_access_url() -> Result<Option<String>> {
    use anyhow::Context;
    use std::process::Command;

    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
        .output()
        .context("launching /usr/bin/security")?;
    if !out.status.success() {
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!url.is_empty()).then_some(url))
}

#[cfg(target_os = "macos")]
pub fn delete_access_url() -> Result<()> {
    use anyhow::Context;
    use std::process::Command;

    // Already-absent is fine — disconnect is idempotent.
    Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT])
        .output()
        .context("launching /usr/bin/security")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn store_access_url(_url: &str) -> Result<()> {
    anyhow::bail!("SimpleFIN credential storage requires the macOS Keychain")
}

#[cfg(not(target_os = "macos"))]
pub fn load_access_url() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_access_url() -> Result<()> {
    Ok(())
}
