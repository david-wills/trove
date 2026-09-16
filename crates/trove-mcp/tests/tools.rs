//! One test per tool against a temp vault seeded with the spec's own example
//! lines, plus the jail test (`.trove/` and `..` refused) and one wire-level
//! test that drives the real binary over stdio.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rmcp::handler::server::wrapper::Parameters;
use serde_json::{json, Value};
use trove_core::Vault;
use trove_mcp::server::{
    DescribeTypeParams, HealthSeriesParams, ReadArtifactParams, ReadStreamParams,
    SearchArtifactsParams,
};
use trove_mcp::TroveServer;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../trove-core/tests/fixtures/spec")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A fresh vault holding the spec example lines in their contract layouts,
/// one note, a secret under `.trove/`, and a tiny Apple Health metric.
fn seeded(name: &str) -> (PathBuf, TroveServer) {
    let root = std::env::temp_dir().join(format!("trove-mcp-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    // Spec examples: correspondence is month-partitioned per source; the
    // fixture's two lines are both 2026-06, so put them in one file and
    // add an older month so pagination and bounds have two partitions.
    write(&root, "correspondence/imessage/2026-06.jsonl", &fixture("correspondence.message.jsonl"));
    write(
        &root,
        "correspondence/imessage/2026-05.jsonl",
        "{\"ts\":\"2026-05-01T08:00:00-07:00\",\"source\":\"imessage\",\"chat\":\"c\",\"sender\":\"s\",\"kind\":\"message\",\"text\":\"older\"}\n",
    );
    write(&root, "calendar/events/2026-03.jsonl", &fixture("calendar.occurrence.jsonl"));
    write(&root, "tasks/ticktick/tasks.jsonl", &fixture("tasks.task.jsonl"));
    write(&root, "artifacts/trip.md", "# Trip planning\n\nbook the flights\n");
    write(&root, ".trove/oauth/google.json", "{\"refresh_token\":\"SECRET\"}\n");
    write(&root, ".trove/secrets/2026-06.jsonl", "{\"token\":\"SECRET\"}\n");
    write(
        &root,
        ".trove/health-summary.json",
        &json!({
            "imported_at": "2026-06-10T00:00:00-07:00",
            "source": "export.zip",
            "metrics": [{
                "slug": "steps", "name": "Steps", "unit": "count", "kind": "sum",
                "records": 3, "first_date": "2026-06-01", "last_date": "2026-06-03"
            }]
        })
        .to_string(),
    );
    write(
        &root,
        "health/steps/daily.csv",
        "date,count,sum,min,max,avg\n2026-06-01,10,1000,50,200,100\n2026-06-02,10,2000,50,300,200\n2026-06-03,10,3000,50,400,300\n",
    );
    let vault = Vault::open_or_create(root.clone()).unwrap();
    (root, TroveServer::new(vault))
}

fn body(r: Result<rmcp::Json<Value>, rmcp::ErrorData>) -> Value {
    r.expect("tool succeeded").0
}

#[tokio::test]
async fn list_sources_reports_registry_and_data_presence() {
    let (_root, s) = seeded("sources");
    let out = body(s.list_sources().await);
    let rows = out["sources"].as_array().unwrap();
    assert!(rows.len() > 10, "registry is non-trivial");
    let im = rows.iter().find(|r| r["id"] == "imessage").unwrap();
    assert_eq!(im["vault_path"], "correspondence/imessage/");
    assert_eq!(im["has_data"], true);
    assert_eq!(im["domain"], "correspondence");
    let oura = rows.iter().find(|r| r["id"] == "oura").unwrap();
    assert_eq!(oura["has_data"], false);
    assert_eq!(oura["connection"], "oura");
}

#[tokio::test]
async fn list_streams_finds_partitioned_dirs_only() {
    let (_root, s) = seeded("streams");
    let out = body(s.list_streams().await);
    let out = &out["streams"];
    let dirs: Vec<&str> = out.as_array().unwrap().iter().map(|r| r["dir"].as_str().unwrap()).collect();
    assert_eq!(dirs, vec!["calendar/events", "correspondence/imessage", "tasks/ticktick"]);
    let im = &out[1];
    assert_eq!(im["files"], 2);
    assert_eq!(im["first"], "2026-05");
    assert_eq!(im["last"], "2026-06");
    assert_eq!(im["dated"], true);
    assert_eq!(im["domain"], "correspondence");
    // The snapshot file is a stream too, just not a dated one.
    assert_eq!(out[2]["dated"], false);
    assert!(!dirs.iter().any(|d| d.starts_with(".trove")));
}

#[tokio::test]
async fn read_stream_pages_newest_first_within_bounds() {
    let (_root, s) = seeded("read");
    let page = |dir: &str, from: Option<&str>, to: Option<&str>, limit: Option<u32>, offset: Option<u32>| {
        Parameters(ReadStreamParams {
            dir: dir.into(),
            from: from.map(Into::into),
            to: to.map(Into::into),
            limit,
            offset,
        })
    };
    let all = body(s.read_stream(page("correspondence/imessage", None, None, None, None)).await);
    assert_eq!(all["records"].as_array().unwrap().len(), 3);
    assert_eq!(all["partitions"], json!(["2026-06", "2026-05"]));
    assert_eq!(all["next_offset"], Value::Null);
    // Newest partition first, and within it file order reversed.
    assert_eq!(all["records"][0]["source"], "email");
    assert_eq!(all["records"][2]["text"], "older");

    let first = body(s.read_stream(page("correspondence/imessage", None, None, Some(2), None)).await);
    assert_eq!(first["records"].as_array().unwrap().len(), 2);
    assert_eq!(first["next_offset"], 2);
    let rest = body(s.read_stream(page("correspondence/imessage", None, None, Some(2), Some(2))).await);
    assert_eq!(rest["records"].as_array().unwrap().len(), 1);
    assert_eq!(rest["next_offset"], Value::Null);

    // A day bound keeps the month that contains it.
    let june = body(s.read_stream(page("correspondence/imessage", Some("2026-06-10"), Some("2026-06-10"), None, None)).await);
    assert_eq!(june["partitions"], json!(["2026-06"]));
    assert_eq!(june["records"].as_array().unwrap().len(), 2);

    assert!(s.read_stream(page("correspondence/imessage", Some("June"), None, None, None)).await.is_err());
    assert!(s.read_stream(page("correspondence/imessage", None, None, Some(0), None)).await.is_err());
    assert!(s.read_stream(page("correspondence/imessage", None, None, Some(5000), None)).await.is_err());
}

#[tokio::test]
async fn describe_type_returns_spec_and_contract() {
    let (_root, s) = seeded("describe");
    let p = |d: Option<&str>| Parameters(DescribeTypeParams { domain: d.map(Into::into) });
    let corr = body(s.describe_type(p(Some("correspondence"))));
    assert_eq!(corr["domain"], "correspondence");
    assert_eq!(corr["contract"]["layout"], "correspondence/<source>/YYYY-MM.jsonl");
    assert!(corr["contract"]["required"].as_array().unwrap().contains(&json!("ts")));
    assert!(corr["markdown"].as_str().unwrap().contains("# Domain: correspondence"));

    let health = body(s.describe_type(p(Some("health"))));
    assert!(health["markdown"].as_str().unwrap().contains("health_series"));
    assert_eq!(health["contract"], Value::Null);

    let index = body(s.describe_type(p(None)));
    let ids = index["domains"].as_array().unwrap();
    assert!(ids.contains(&json!("tasks")) && ids.contains(&json!("conventions")));
    assert!(index["contracts"].as_array().unwrap().len() >= 4);

    assert!(s.describe_type(p(Some("nope"))).is_err());
}

#[tokio::test]
async fn artifacts_search_and_read() {
    let (_root, s) = seeded("artifacts");
    let hits = body(s.search_artifacts(Parameters(SearchArtifactsParams { query: "FLIGHTS".into() })).await);
    let hits = &hits["artifacts"];
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["path"], "artifacts/trip.md");
    assert_eq!(hits[0]["title"], "Trip planning");
    let doc = body(s.read_artifact(Parameters(ReadArtifactParams { path: "artifacts/trip.md".into() })).await);
    assert!(doc["content"].as_str().unwrap().contains("book the flights"));
    let none = body(s.search_artifacts(Parameters(SearchArtifactsParams { query: "zzz".into() })).await);
    assert!(none["artifacts"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn health_metrics_and_series() {
    let (_root, s) = seeded("health");
    let metrics = body(s.health_metrics().await);
    let steps = metrics["metrics"].as_array().unwrap().iter().find(|m| m["slug"] == "steps").unwrap();
    assert_eq!(steps["sources"][0]["source"], "apple-health");
    assert_eq!(steps["sources"][0]["records"], 3);

    let series = |bucket: Option<&str>| {
        Parameters(HealthSeriesParams { metric: "steps".into(), bucket: bucket.map(Into::into) })
    };
    let daily = body(s.health_series(series(None)).await);
    assert_eq!(daily["bucket"], "day");
    let points = daily["series"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 3);
    assert_eq!(points[1]["value"], 2000.0);
    let monthly = body(s.health_series(series(Some("month"))).await);
    assert_eq!(monthly["series"][0]["points"][0]["value"], 6000.0);
    assert!(s.health_series(series(Some("year"))).await.is_err());
    assert!(s
        .health_series(Parameters(HealthSeriesParams { metric: "nope".into(), bucket: None }))
        .await
        .is_err());
}

#[tokio::test]
async fn trove_dir_and_escapes_are_refused_everywhere() {
    let (root, s) = seeded("jail");
    let outside = root.parent().unwrap().join("trove-mcp-outside.jsonl");
    fs::write(&outside, "{\"leak\":1}\n").unwrap();
    for dir in [
        ".trove",
        ".trove/secrets",
        ".Trove/secrets",
        "./.trove/secrets",
        "correspondence/../.trove/secrets",
        "..",
        "../",
        "/etc",
        root.to_str().unwrap(),
    ] {
        let r = s
            .read_stream(Parameters(ReadStreamParams {
                dir: dir.into(),
                from: None,
                to: None,
                limit: None,
                offset: None,
            }))
            .await;
        assert!(r.is_err(), "read_stream({dir:?}) must be refused");
    }
    for path in [".trove/oauth/google.json", "../trove-mcp-outside.jsonl", "/etc/hosts", ".Trove/oauth/google.json"] {
        let r = s.read_artifact(Parameters(ReadArtifactParams { path: path.into() })).await;
        assert!(r.is_err(), "read_artifact({path:?}) must be refused");
    }
    // And nothing under .trove is ever listed as a stream.
    let streams = body(s.list_streams().await);
    assert!(streams["streams"].as_array().unwrap().iter().all(|r| !r["dir"].as_str().unwrap().contains(".trove")));
    let _ = fs::remove_file(outside);
}

/// Drive the real binary over stdio: initialize, list tools, call one.
#[test]
fn binary_speaks_mcp_over_stdio() {
    let (root, _s) = seeded("stdio");
    let mut child = Command::new(env!("CARGO_BIN_EXE_trove-mcp"))
        .arg("--vault")
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn trove-mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: Value| {
        stdin.write_all(v.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };
    let mut recv = || -> Value {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
    };

    send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}));
    let init = recv();
    assert_eq!(init["result"]["serverInfo"]["name"], "trove-mcp");
    assert!(init["result"]["instructions"].as_str().unwrap().contains(root.to_str().unwrap()));
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));

    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let list = recv();
    let mut names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "describe_type",
            "health_metrics",
            "health_series",
            "list_sources",
            "list_streams",
            "read_artifact",
            "read_stream",
            "search_artifacts",
        ]
    );

    send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"read_stream","arguments":{"dir":"calendar/events","limit":1}}}));
    let call = recv();
    let structured = &call["result"]["structuredContent"];
    // Newest first: the fixture's last line comes back first.
    let last_title = fixture("calendar.occurrence.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["title"].clone())
        .unwrap();
    assert_eq!(structured["records"][0]["title"], last_title);
    assert_eq!(structured["next_offset"], 1);
    // The text block carries the same JSON for clients without structured output.
    let text: Value = serde_json::from_str(call["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["partitions"], json!(["2026-03"]));

    // Every tool's structuredContent is a JSON object (MCP requires it; a
    // top-level array fails client-side validation).
    for (id, name) in [(10, "list_sources"), (11, "list_streams"), (12, "health_metrics"), (13, "search_artifacts")] {
        send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":{}}}));
        let r = recv();
        assert!(r["result"]["structuredContent"].is_object(), "{name}: {r}");
    }

    send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
        "name":"read_stream","arguments":{"dir":".trove/secrets"}}}));
    let refused = recv();
    assert!(refused["error"].is_object() || refused["result"]["isError"] == true, "{refused}");

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "server exits cleanly on EOF: {status}");
}
