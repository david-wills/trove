//! Anti-drift harness for `docs/integration-schedule.md`.
//!
//! The schedule table is *generated* from [`trove_core::INTEGRATIONS`]: this
//! test renders it and asserts the committed file matches byte-for-byte, so
//! a def change (cadence, gate, shape, new integration) without a doc
//! regeneration fails CI. To regenerate instead of asserting:
//!
//! ```bash
//! TROVE_REGEN=1 cargo test -p trove-core --test schedule_doc
//! ```

use std::fs;
use std::path::Path;

use trove_core::{Advance, Behavior, Cadence, Gate, INTEGRATIONS, POLL_SECS};

const DOC_PATH: &str = "../../docs/integration-schedule.md";
const REGEN_CMD: &str = "TROVE_REGEN=1 cargo test -p trove-core --test schedule_doc";

/// "every 15 min" / "hourly" / "every 2 hours" / "every 90 s".
fn period(secs: u64) -> String {
    if secs == 3600 {
        "hourly".into()
    } else if secs % 3600 == 0 {
        format!("every {} hours", secs / 3600)
    } else if secs % 60 == 0 {
        format!("every {} min", secs / 60)
    } else {
        format!("every {secs} s")
    }
}

/// Plain-English reading of one periodic cadence, exactly as the runner's
/// `JobState` treats it.
fn schedule(cadence: &Cadence) -> String {
    let base = period(cadence.every_secs);
    let mut s = match cadence.gate {
        Gate::Always => base,
        Gate::LocalDay => format!("checked {base}, runs once per local day"),
        Gate::SourceMtime(_) => format!(
            "checked {base}, runs only when the source database changed; \
             retries on failure without burning the gate"
        ),
    };
    if cadence.advance == Advance::Run {
        s.push_str(
            "; timer only advances when it actually runs, so re-enabling fires immediately",
        );
    }
    s
}

fn render() -> String {
    let mut out = String::new();
    out.push_str("# Integration schedule\n\n");
    out.push_str("<!-- GENERATED — do not edit by hand. -->\n\n");
    out.push_str(
        "Generated from `INTEGRATIONS` (each integration's `Behavior` in its \
         `IntegrationDef`).\nThe `schedule_doc` test pins this file to the registry \
         byte-for-byte; after\nchanging a def, regenerate with:\n\n",
    );
    out.push_str(&format!("```bash\n{REGEN_CMD}\n```\n\n"));
    out.push_str("| Id | Name | Shape | Schedule |\n|---|---|---|---|\n");
    for d in INTEGRATIONS {
        let (shape, sched) = match &d.behavior {
            Behavior::Periodic { cadence, .. } => ("Periodic".to_string(), schedule(cadence)),
            Behavior::CoveredBy(owner) => (
                format!("Covered by `{owner}`"),
                format!("runs with `{owner}`'s pass"),
            ),
            Behavior::Live(_) => (
                "Live".to_string(),
                format!("always on; ticked every poll ({POLL_SECS} s)"),
            ),
            Behavior::NativeHost => (
                "Native host".to_string(),
                "event-driven; the browser extension's native-messaging host writes as events arrive"
                    .to_string(),
            ),
            Behavior::Import(_) => (
                "Import".to_string(),
                "manual; runs when you import a file".to_string(),
            ),
            Behavior::NotWired => ("Not wired".to_string(), "nothing runs yet".to_string()),
            Behavior::Unavailable { .. } => (
                "Unavailable".to_string(),
                "nothing runs — catalogued with the reason shown in-app".to_string(),
            ),
        };
        out.push_str(&format!("| `{}` | {} | {} | {} |\n", d.id, d.name, shape, sched));
    }
    out
}

#[test]
fn schedule_doc_matches_registry() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC_PATH);
    let want = render();
    if std::env::var("TROVE_REGEN").as_deref() == Ok("1") {
        fs::write(&path, &want).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }
    let got = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} — generate it with `{REGEN_CMD}`", path.display()));
    assert_eq!(
        got, want,
        "docs/integration-schedule.md is stale — regenerate with `{REGEN_CMD}`"
    );
}
