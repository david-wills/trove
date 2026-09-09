//! The M6 proof: the *actual Python example* in
//! `docs/vault-spec/writing-a-collector.md` — extracted from the doc, run
//! verbatim against a temp vault — produces files the tasks reader picks up
//! with no registration. If the doc's example ever drifts from what the
//! readers accept, this fails.

use std::fs;
use std::path::Path;
use std::process::Command;

use trove_core::{TaskEvent, Vault};

#[test]
fn the_docs_python_collector_works_against_a_real_vault() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let doc = fs::read_to_string(repo.join("docs/vault-spec/writing-a-collector.md")).unwrap();
    let fence = "```python\n";
    let start = doc.find(fence).expect("doc has a python example") + fence.len();
    let end = doc[start..].find("```").unwrap() + start;
    let script = &doc[start..end];

    let home = std::env::temp_dir().join(format!("trove-m6-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();

    // The example targets ~/Trove; pathlib honors $HOME.
    let out = match Command::new("python3").arg("-c").arg(script).env("HOME", &home).output() {
        Ok(out) => out,
        Err(e) => {
            eprintln!("skipping: python3 unavailable ({e})");
            return;
        }
    };
    assert!(
        out.status.success(),
        "doc example failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The vault sees the new source with no registration anywhere.
    let vault = Vault::open_or_create(home.join("Trove")).unwrap();
    let tasks = vault.load_tasks_snapshot("my-todo-script").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "42");
    assert_eq!(tasks[0].title, "Water the plants");
    assert_eq!(tasks[0].status, "open", "defaults filled for the sparse line");

    // The completion event parses as the contract type.
    let events_dir = home.join("Trove/tasks/my-todo-script/events");
    let month_file = fs::read_dir(&events_dir).unwrap().next().unwrap().unwrap().path();
    let line = fs::read_to_string(month_file).unwrap();
    let event: TaskEvent = serde_json::from_str(line.lines().next().unwrap()).unwrap();
    assert_eq!(event.kind, "completed");
    assert_eq!(event.task.title, "Buy seeds");
}
