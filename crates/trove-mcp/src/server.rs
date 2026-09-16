//! The MCP server: one struct, eight read-only tools, each a thin wrapper
//! over a `trove-core` read path run on a blocking thread.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ErrorData, Json, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use trove_core::{Bucket, Vault, DOMAINS, STREAM_PAGE_MAX};

use crate::spec;

/// Default page size for `read_stream` when the caller gives none.
const DEFAULT_LIMIT: u32 = 100;

/// Byte budget for one `read_stream` page. MCP clients cap a tool result
/// (Claude Code refuses very large ones outright), and some streams carry
/// big records (email bodies), so a page ends early once its records pass
/// this and `next_offset` points at the rest.
const PAGE_BYTES: usize = 160_000;

#[derive(Clone)]
pub struct TroveServer {
    vault: Arc<Vault>,
    /// Read by the generated `call_tool` in the `#[tool_handler]` impl.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadStreamParams {
    /// Vault-relative stream directory as returned by `list_streams`,
    /// e.g. "correspondence/imessage", "activity", "calendar/events".
    pub dir: String,
    /// Inclusive lower bound on the partition date, "YYYY-MM-DD" or
    /// "YYYY-MM". A month-partitioned stream keeps the month containing it.
    #[serde(default)]
    pub from: Option<String>,
    /// Inclusive upper bound on the partition date, same forms as `from`.
    #[serde(default)]
    pub to: Option<String>,
    /// Records per page (default 100, max 1000). Newest first.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Records to skip; pass the previous page's `next_offset`.
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTypeParams {
    /// Domain id: a contract domain ("correspondence", "tasks", "calendar",
    /// "media-plays", …), "health" for the health layout, or "conventions"
    /// for the vault-wide invariants. Omit to list every id.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArtifactsParams {
    /// Case-insensitive substring matched against title, filename, and
    /// content. Empty lists every artifact, newest first.
    #[serde(default)]
    pub query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArtifactParams {
    /// Vault-relative path from `search_artifacts`, e.g. "artifacts/2026-06-10-thoughts.md".
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HealthSeriesParams {
    /// Metric slug from `health_metrics`, e.g. "steps", "sleep", "hrv".
    pub metric: String,
    /// Aggregation bucket: "day" (default), "week", or "month".
    #[serde(default)]
    pub bucket: Option<String>,
}

fn internal(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}

fn invalid(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

#[tool_router]
impl TroveServer {
    pub fn new(vault: Vault) -> Self {
        Self { vault: Arc::new(vault), tool_router: Self::tool_router() }
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Run a vault read on a blocking thread — vault I/O never runs on the
    /// async runtime, same rule as the app's Tauri commands. Every result
    /// must serialize to a JSON *object*: MCP's `structuredContent` is an
    /// object, and clients reject a top-level array (so list-shaped reads
    /// wrap themselves: `{ "streams": [...] }`).
    async fn blocking<T>(
        &self,
        f: impl FnOnce(&Vault) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<Json<Value>, ErrorData>
    where
        T: serde::Serialize + Send + 'static,
    {
        let vault = self.vault.clone();
        let out = tokio::task::spawn_blocking(move || f(&vault))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .map_err(internal)?;
        serde_json::to_value(out).map(Json).map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_sources",
        description = "Every data source Trove knows (its integration registry): id, name, kind (live / local-sync / cloud-sync / import), domain, the vault directory it writes, whether that directory has any data, and when data last landed. Returns {sources: [...]}. Start here to learn what this vault can answer."
    )]
    pub async fn list_sources(&self) -> Result<Json<Value>, ErrorData> {
        self.blocking(|v| Ok(json!({ "sources": trove_core::list_sources(v) }))).await
    }

    #[tool(
        name = "list_streams",
        description = "Every directory of JSONL records in the vault with its file count, byte size, first/last partition key (YYYY-MM or YYYY-MM-DD), whether it is date-partitioned, and the vault-spec contract it belongs to. Cheap (directory listings only). Returns {streams: [...]}; use the `dir` values with read_stream."
    )]
    pub async fn list_streams(&self) -> Result<Json<Value>, ErrorData> {
        self.blocking(|v| Ok(json!({ "streams": v.list_streams()? }))).await
    }

    #[tool(
        name = "read_stream",
        description = "Newest-first raw records from one stream directory, paginated. from/to (inclusive, YYYY-MM-DD or YYYY-MM) pick the partition files to read, and a day-precision bound also filters records by their own date (ts/occurrence/start/day), so one day of a month-partitioned stream is one day. A page ends early when its records exceed ~160 KB (large email bodies); always follow next_offset until it is null — a short page is not the end. Returns {records, partitions, next_offset}. Records are the vault's own JSON lines; call describe_type first for the field meanings. Timestamps are RFC3339 local time."
    )]
    pub async fn read_stream(
        &self,
        Parameters(p): Parameters<ReadStreamParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 || limit as usize > STREAM_PAGE_MAX {
            return Err(invalid(format!("limit must be 1..={STREAM_PAGE_MAX}")));
        }
        for (name, bound) in [("from", &p.from), ("to", &p.to)] {
            if let Some(b) = bound {
                if !is_date_key(b) {
                    return Err(invalid(format!("{name} must be YYYY-MM-DD or YYYY-MM, got {b:?}")));
                }
            }
        }
        self.blocking(move |v| {
            v.read_stream_page(
                &p.dir,
                p.from.as_deref(),
                p.to.as_deref(),
                limit as usize,
                p.offset.unwrap_or(0) as usize,
                Some(PAGE_BYTES),
            )
        })
        .await
    }

    #[tool(
        name = "describe_type",
        description = "The vault-spec page for a domain — the record shape, required fields, layout, and example lines — so you know what read_stream will return before reading. Pass \"health\" for the health/ layout or \"conventions\" for vault-wide rules (timestamps, partitions, `extra`). Omit `domain` to list every id."
    )]
    pub fn describe_type(
        &self,
        Parameters(p): Parameters<DescribeTypeParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let contracts: Vec<Value> = DOMAINS
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "layout": c.layout,
                    "root": c.root,
                    "kind": c.kind.as_str(),
                    "required": c.required,
                    "spec_page": c.spec_page,
                })
            })
            .collect();
        let Some(domain) = p.domain.as_deref().map(str::trim).filter(|d| !d.is_empty()) else {
            return Ok(Json(json!({ "domains": spec::ids(), "contracts": contracts })));
        };
        let Some(page) = spec::page(domain) else {
            return Err(invalid(format!(
                "unknown domain {domain:?}; known: {}",
                spec::ids().join(", ")
            )));
        };
        let contract = contracts
            .iter()
            .find(|c| c["id"] == page.id)
            .cloned()
            .unwrap_or(Value::Null);
        Ok(Json(json!({
            "domain": page.id,
            "spec_page": page.path,
            "contract": contract,
            "markdown": page.markdown,
        })))
    }

    #[tool(
        name = "search_artifacts",
        description = "Search the notes layer (artifacts/: authored markdown notes and imported .md/.txt documents) by case-insensitive substring over title, filename, and content. Returns {artifacts: [{path, title, modified}, ...]}, newest first. Read one with read_artifact."
    )]
    pub async fn search_artifacts(
        &self,
        Parameters(p): Parameters<SearchArtifactsParams>,
    ) -> Result<Json<Value>, ErrorData> {
        self.blocking(move |v| Ok(json!({ "artifacts": v.search_artifacts(&p.query)? }))).await
    }

    #[tool(
        name = "read_artifact",
        description = "The full text of one artifact by its vault-relative path (from search_artifacts)."
    )]
    pub async fn read_artifact(
        &self,
        Parameters(p): Parameters<ReadArtifactParams>,
    ) -> Result<Json<Value>, ErrorData> {
        self.blocking(move |v| {
            let content = v.read_artifact(&p.path)?;
            Ok(json!({ "path": p.path, "content": content }))
        })
        .await
    }

    #[tool(
        name = "health_metrics",
        description = "Every chartable health metric across all sources (Apple Health import, Oura sync), overlaps merged under one canonical slug: slug, name, unit, aggregation kind, a methodology note when sources differ, and per-source record counts and date ranges. Returns {metrics: [...]}; feed a slug to health_series."
    )]
    pub async fn health_metrics(&self) -> Result<Json<Value>, ErrorData> {
        self.blocking(|v| Ok(json!({ "metrics": v.health_metrics_unified()? }))).await
    }

    #[tool(
        name = "health_series",
        description = "Time series for one health metric, aggregated per day/week/month in the vault (never more than a few thousand points). Returns {metric, bucket, series: [{source, points: [{date, value}]}]} — one series per source so you can compare them; sources are not merged."
    )]
    pub async fn health_series(
        &self,
        Parameters(p): Parameters<HealthSeriesParams>,
    ) -> Result<Json<Value>, ErrorData> {
        let bucket = match p.bucket.as_deref().map(str::trim).unwrap_or("day") {
            "day" => Bucket::Day,
            "week" => Bucket::Week,
            "month" => Bucket::Month,
            other => return Err(invalid(format!("bucket must be day, week, or month; got {other:?}"))),
        };
        self.blocking(move |v| {
            Ok(json!({ "metric": p.metric, "bucket": bucket, "series": v.health_series_unified(&p.metric, bucket)? }))
        })
        .await
    }
}

/// `YYYY-MM-DD` or `YYYY-MM`.
fn is_date_key(s: &str) -> bool {
    let ok = |n: usize| {
        s.len() == n
            && s.bytes()
                .enumerate()
                .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
    };
    ok(10) || ok(7)
}

#[tool_handler]
impl ServerHandler for TroveServer {
    fn get_info(&self) -> ServerConfig {
        let root = self.vault.root().display();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("trove-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Trove is a local, private personal-data vault at {root}: plain JSONL, CSV, and \
                 markdown files written by many collectors (messages, browser history, calendar, \
                 activity, health, music, finance, …). This server is read-only.\n\n\
                 Workflow: call list_sources to see what exists and which sources have data, \
                 list_streams for every readable JSONL directory with its date range, then \
                 describe_type(domain) to learn a record shape before read_stream(dir, from, to). \
                 read_stream is newest-first and paginated — bound it with from/to and keep \
                 following next_offset rather than asking for huge pages. Health metrics (Apple \
                 Health + Oura) are pre-aggregated: health_metrics then health_series(metric, \
                 bucket). Notes and documents: search_artifacts then read_artifact.\n\n\
                 Timestamps are RFC3339 local time; `source` names the collector; anything a \
                 shared shape has no column for is under `extra`. Cite the stream and date range \
                 you read when answering."
            ))
    }
}
