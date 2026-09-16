use std::collections::BTreeMap;
use std::sync::Mutex;

use tauri::{AppHandle, Emitter, State};
use trove_core::{
    ActivityEvent, ActivitySummary, AdRecord, AdsDaily, AdsSummary, ArtifactMeta, BrowserSummary,
    BrowserSyncState, BrowserVisit, Bucket, DeviceInfo, LiveSpan, MediaItem,
    MediaSummary, MetricSummary, MusicSummary, Play, ScreenTimeSession, ScreenTimeSummary,
    SeriesPoint, Task, TasksOverview, TasksSyncState, Vault, WatchControl, WatcherRole,
};

/// The open vault, shared across commands.
struct AppState {
    vault: Mutex<Vault>,
    /// Handle to the contend-and-watch loop (see `trove_core::runner`): tells
    /// us whether this process owns collection and what the live event is.
    watch: WatchControl,
}

#[derive(serde::Serialize, specta::Type)]
struct VaultInfo {
    root: String,
}

#[tauri::command]
#[specta::specta]
fn vault_info(state: State<AppState>) -> Result<VaultInfo, String> {
    let vault = state.vault.lock().unwrap();
    Ok(VaultInfo {
        root: vault.root().to_string_lossy().into_owned(),
    })
}

#[tauri::command]
#[specta::specta]
async fn list_artifacts(state: State<'_, AppState>) -> Result<Vec<ArtifactMeta>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.list_artifacts().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn read_artifact(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.read_artifact(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn write_artifact(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.write_artifact(&path, &content).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn create_artifact(state: State<'_, AppState>, title: String) -> Result<ArtifactMeta, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.create_artifact(&title).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn delete_artifact(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.delete_artifact(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Copy dropped files (absolute paths from the OS drag) into artifacts/.
#[tauri::command]
#[specta::specta]
async fn import_artifacts(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<ArtifactMeta>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let sources: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.import_artifacts(&sources).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn search_artifacts(state: State<'_, AppState>, query: String) -> Result<Vec<ArtifactMeta>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.search_artifacts(&query).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Progress payload of the "import-progress" events, tagged by integration
/// so one listener serves every import.
#[derive(Clone, serde::Serialize, specta::Type)]
struct ImportEvent {
    integration_id: String,
    records: u64,
    percent: f32,
}

/// Run any catalog-declared file import (`IntegrationDef::import`) on a
/// blocking thread with its own Vault handle, reporting progress via
/// "import-progress" events. One command serves every import — a new import
/// is a catalog entry plus a parser, never a new command.
#[tauri::command]
#[specta::specta]
async fn run_import(
    app: AppHandle,
    state: State<'_, AppState>,
    integration_id: String,
    path: String,
    params: BTreeMap<String, String>,
) -> Result<trove_core::ImportOutcome, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let def = trove_core::INTEGRATIONS
            .iter()
            .find(|d| d.id == integration_id)
            .ok_or_else(|| format!("unknown integration: {integration_id}"))?;
        let spec = def
            .import_spec()
            .ok_or_else(|| format!("{integration_id} is not a file import"))?;
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let mut last = -1.0f32;
        (spec.run)(&vault, std::path::Path::new(&path), &params, &mut |p| {
            if p.percent - last >= 0.5 {
                last = p.percent;
                let _ = app.emit(
                    "import-progress",
                    ImportEvent {
                        integration_id: integration_id.clone(),
                        records: p.records,
                        percent: p.percent,
                    },
                );
            }
        })
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Rebuild and return the vault manifest: every data folder present, with
/// sources and date ranges — the generic data browser's map.
#[tauri::command]
#[specta::specta]
async fn vault_manifest(state: State<'_, AppState>) -> Result<trove_core::Manifest, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.rebuild_manifest().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Newest-first raw records from any date-partitioned JSONL directory — the
/// generic "Recent data" read behind every integration card. One thin wrapper
/// over [`Vault::read_stream_page`] (shared with `trove-mcp`), which jails
/// path escapes and `.trove/`.
#[tauri::command]
#[specta::specta]
async fn read_stream(
    state: State<'_, AppState>,
    dir: String,
    limit: u32,
    offset: u32,
) -> Result<trove_core::StreamPage, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .read_stream_page(&dir, None, None, limit as usize, offset as usize, None)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ===========================================================================
// The drop-in normalizer (R2) — detect a dropped file's fate, confirm a
// source→contract mapping, project it, and browse raw drops. Every command is
// a thin async wrapper over `trove_core::normalizer`; the rich mapping/detection
// types cross the boundary as `JsonValue` (they carry an untagged enum and
// arbitrary `serde_json::Value` cells that don't derive `specta::Type`), typed
// precisely on the frontend in `src/api.ts` — the same shape read_stream uses.
// ===========================================================================

/// The dropped file's detected fate: `Detection` serialized (headers, sample
/// rows, format, and the three-outcome `outcome`). No vault I/O — reads only
/// the dropped file at the given absolute OS path.
#[tauri::command]
#[specta::specta]
async fn normalizer_detect(path: String) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let det = trove_core::normalizer::detect(std::path::Path::new(&path))
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_value(det).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The exact consent payload for an LLM suggestion on this file: `{ payload,
/// request_body }` where `payload` is the [`trove_core::normalizer::SuggestPayload`]
/// (headers + ≤5 sample rows + candidate contract metadata) and `request_body`
/// is the literal Anthropic request body. The UI shows this verbatim before any
/// network call — there is no "always allow".
#[tauri::command]
#[specta::specta]
async fn normalizer_suggest_payload(path: String) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let det = trove_core::normalizer::detect(std::path::Path::new(&path))
            .map_err(|e| format!("{e:#}"))?;
        let payload = trove_core::normalizer::SuggestPayload::from_detection(&det);
        let body = payload.request_body();
        Ok(serde_json::json!({ "payload": payload, "request_body": body }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether the LLM advisor is usable (BYO or baked key), with the reason when
/// not — the gate the suggest control renders disabled against. No network.
#[tauri::command]
#[specta::specta]
async fn normalizer_advisor_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let status = trove_core::normalizer::llm_advisor_status(&vault).map_err(|e| format!("{e:#}"))?;
        serde_json::to_value(status).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run the opt-in cloud advisor after the user has consented, returning a draft
/// [`trove_core::normalizer::Mapping`] that pre-fills the binding form. The
/// payload is rebuilt from the file deterministically (detect → `from_detection`),
/// so it is byte-identical to what `normalizer_suggest_payload` displayed in the
/// consent dialog — what was shown is exactly what is sent. Errors if no key is
/// configured.
#[tauri::command]
#[specta::specta]
async fn normalizer_suggest(
    state: State<'_, AppState>,
    source: String,
    path: String,
) -> Result<serde_json::Value, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let det = trove_core::normalizer::detect(std::path::Path::new(&path))
            .map_err(|e| format!("{e:#}"))?;
        let payload = trove_core::normalizer::SuggestPayload::from_detection(&det);
        let mapping = trove_core::normalizer::llm_suggest(&vault, &source, &payload)
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_value(mapping).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A dry run of a (possibly-edited) mapping against the dropped file: applies it
/// without writing, returning row counts and a small sample for the validation
/// preview. Pure — no vault I/O beyond reading the dropped file.
#[tauri::command]
#[specta::specta]
async fn normalizer_preview(path: String, mapping: serde_json::Value) -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let m: trove_core::normalizer::Mapping =
            serde_json::from_value(mapping).map_err(|e| format!("bad mapping: {e}"))?;
        m.validate().map_err(|e| format!("{e:#}"))?;
        let applied = m.apply(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))?;
        let sample: Vec<&serde_json::Map<String, serde_json::Value>> =
            applied.rows.iter().take(10).collect();
        Ok(serde_json::json!({
            "total": applied.total,
            "valid": applied.valid,
            "invalid": applied.invalid,
            "invalid_samples": applied.invalid_samples,
            "sample_rows": sample,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Confirm a mapping: persist it under `.trove/mappings/<source>.json`, land the
/// dropped file raw, and project its rows into the domain folders (guid-merged).
/// Progress streams on the "import-progress" event tagged `normalizer`.
#[tauri::command]
#[specta::specta]
async fn normalizer_confirm(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    mapping: serde_json::Value,
) -> Result<trove_core::ImportOutcome, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let m: trove_core::normalizer::Mapping =
            serde_json::from_value(mapping).map_err(|e| format!("bad mapping: {e}"))?;
        m.save(&vault).map_err(|e| format!("{e:#}"))?;
        let mut last = -1.0f32;
        m.project(&vault, std::path::Path::new(&path), &mut |p| {
            if p.percent - last >= 0.5 {
                last = p.percent;
                let _ = app.emit(
                    "import-progress",
                    ImportEvent {
                        integration_id: "normalizer".to_string(),
                        records: p.records,
                        percent: p.percent,
                    },
                );
            }
        })
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Re-project a source from raw after its mapping was edited: delete the
/// projected partitions, rewrite from every raw drop. Idempotent.
#[tauri::command]
#[specta::specta]
async fn normalizer_reproject(
    state: State<'_, AppState>,
    source: String,
) -> Result<trove_core::ImportOutcome, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        trove_core::normalizer::Mapping::reproject(&vault, &source).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Every stored mapping, in source order (each a serialized `Mapping`).
#[tauri::command]
#[specta::specta]
async fn normalizer_list_mappings(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let mappings = trove_core::normalizer::Mapping::list(&vault).map_err(|e| format!("{e:#}"))?;
        mappings
            .iter()
            .map(|m| serde_json::to_value(m).map_err(|e| e.to_string()))
            .collect()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Delete a mapping and its projected partitions, keeping raw.
#[tauri::command]
#[specta::specta]
async fn normalizer_delete_mapping(state: State<'_, AppState>, source: String) -> Result<(), String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        trove_core::normalizer::Mapping::delete(&vault, &source).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Land a nothing-fits drop raw under `imports/<source>/`, listed in the
/// declined-drops manifest and browsable in the raw viewer. Returns the entry.
#[tauri::command]
#[specta::specta]
async fn normalizer_decline(
    state: State<'_, AppState>,
    source: String,
    path: String,
) -> Result<serde_json::Value, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let entry = vault
            .land_declined(&source, std::path::Path::new(&path))
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_value(entry).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The declined-drops manifest: every raw file landed under `imports/`.
#[tauri::command]
#[specta::specta]
async fn normalizer_list_declined(state: State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .list_declined()
            .iter()
            .map(|d| serde_json::to_value(d).map_err(|e| e.to_string()))
            .collect()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A page of a raw dropped file rendered as a table (`RawPage` serialized). The
/// path is vault-relative; the authoritative jail (path escapes plus `.trove/`,
/// case-insensitive) lives in core via `Vault::resolve_user`, so it can't be
/// bypassed with a `./` prefix or letter-case variation. Read cost is
/// O(offset + limit), never the whole file.
#[tauri::command]
#[specta::specta]
async fn normalizer_read_raw(
    state: State<'_, AppState>,
    path: String,
    offset: u32,
    limit: u32,
) -> Result<serde_json::Value, String> {
    if path.is_empty() {
        return Err("not a readable file".into());
    }
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        let page = vault
            .read_raw(&path, offset as u64, limit as u64)
            .map_err(|e| format!("{e:#}"))?;
        serde_json::to_value(page).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Save a bring-your-own Anthropic API key for the advisor (0600, BYO wins over
/// any baked key). Rejects an empty key.
#[tauri::command]
#[specta::specta]
async fn normalizer_save_key(state: State<'_, AppState>, key: String) -> Result<(), String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.save_llm_key(&key).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forget the saved BYO advisor key (a baked key, if any, still applies).
#[tauri::command]
#[specta::specta]
async fn normalizer_delete_key(state: State<'_, AppState>) -> Result<(), String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.delete_llm_key().map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
fn list_health_metrics(state: State<AppState>) -> Result<Vec<MetricSummary>, String> {
    let vault = state.vault.lock().unwrap();
    vault.list_health_metrics().map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn health_series(
    state: State<'_, AppState>,
    metric: String,
    bucket: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let bucket = match bucket.as_str() {
            "day" => Bucket::Day,
            "week" => Bucket::Week,
            "month" => Bucket::Month,
            other => return Err(format!("unknown bucket: {other}")),
        };
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.health_series(&metric, bucket).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Every chartable health metric across all sources (Apple Health import +
/// Oura sync), overlaps merged under one canonical slug.
#[tauri::command]
#[specta::specta]
async fn health_metrics_unified(
    state: State<'_, AppState>,
) -> Result<Vec<trove_core::UnifiedMetric>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.health_metrics_unified().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Per-source series for one canonical metric — sources stay separate so the
/// chart overlays them rather than merging.
#[tauri::command]
#[specta::specta]
async fn health_series_unified(
    state: State<'_, AppState>,
    metric: String,
    bucket: String,
) -> Result<Vec<trove_core::SourceSeries>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let bucket = match bucket.as_str() {
            "day" => Bucket::Day,
            "week" => Bucket::Week,
            "month" => Bucket::Month,
            other => return Err(format!("unknown bucket: {other}")),
        };
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .health_series_unified(&metric, bucket)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Latest Oura daily scores (readiness, sleep, activity, stress, resilience,
/// cardio age) for the Overview cards. Empty when Oura has never synced.
#[tauri::command]
#[specta::specta]
async fn oura_overview(state: State<'_, AppState>) -> Result<Vec<trove_core::OuraDayScore>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.oura_overview().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Recent Oura sleep sessions, newest first (naps included, marked by kind).
#[tauri::command]
#[specta::specta]
async fn oura_sleep_nights(
    state: State<'_, AppState>,
    limit: usize,
) -> Result<Vec<trove_core::SleepNight>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.oura_sleep_nights(limit).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Intraday heart-rate samples in an RFC3339 window, downsampled to at most
/// `max_points`.
#[tauri::command]
#[specta::specta]
async fn oura_heartrate_range(
    state: State<'_, AppState>,
    start: String,
    end: String,
    max_points: usize,
) -> Result<Vec<trove_core::HeartratePoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .oura_heartrate_range(&start, &end, max_points)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The newest workouts and sessions across Apple Health and Oura, merged and
/// source-labeled.
#[tauri::command]
#[specta::specta]
async fn health_workouts(
    state: State<'_, AppState>,
    limit: usize,
) -> Result<Vec<trove_core::WorkoutItem>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.health_workouts(limit).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Per-app time plus active/AFK totals over an inclusive date range
/// (YYYY-MM-DD). The in-progress event is added in via `activity_current`.
#[tauri::command]
#[specta::specta]
async fn activity_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<ActivitySummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.activity_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn activity_timeline(state: State<'_, AppState>, date: String) -> Result<Vec<ActivityEvent>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.activity_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn activity_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.activity_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The event currently in progress (not yet written to the log), if any —
/// ours when this app owns collection, otherwise mirrored from the owning
/// process's heartbeat (e.g. the troved daemon).
#[tauri::command]
#[specta::specta]
fn activity_current(state: State<AppState>) -> Option<ActivityEvent> {
    state.watch.current()
}

/// Per-app and per-device screen time over an inclusive date range
/// (YYYY-MM-DD). `device` narrows to one device UUID (or "this-mac");
/// `include_idle: false` hides lock screens, StandBy, and watch faces.
#[tauri::command]
#[specta::specta]
async fn screen_time_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
    device: Option<String>,
    include_idle: bool,
) -> Result<ScreenTimeSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .screen_time_summary(&from, &to, device.as_deref(), include_idle)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn screen_time_timeline(
    state: State<'_, AppState>,
    date: String,
    device: Option<String>,
    include_idle: bool,
) -> Result<Vec<ScreenTimeSession>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .screen_time_timeline(&date, device.as_deref(), include_idle)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn screen_time_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
    device: Option<String>,
    include_idle: bool,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .screen_time_daily(&from, &to, device.as_deref(), include_idle)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The device catalog (UUID → kind/label/last-seen); empty until the first
/// Biome sync lands.
#[tauri::command]
#[specta::specta]
fn screen_time_devices(state: State<AppState>) -> BTreeMap<String, DeviceInfo> {
    let vault = state.vault.lock().unwrap();
    vault.screen_time_devices()
}

/// Whether this process can read the Biome streams (Full Disk Access).
/// Grants are per-binary — troved needs its own; the hub handles that.
#[tauri::command]
#[specta::specta]
fn screen_time_permission() -> bool {
    trove_core::screen_time_permission_ok()
}

#[derive(serde::Serialize, specta::Type)]
struct WatcherStatus {
    /// Who is collecting right now: "app", "daemon", or "none".
    collector: String,
    /// Whether the troved launch agent plist is installed.
    daemon_installed: bool,
}

#[tauri::command]
#[specta::specta]
fn watcher_status(state: State<AppState>) -> WatcherStatus {
    let collector = if state.watch.owns() {
        "app".into()
    } else {
        let vault = state.vault.lock().unwrap();
        vault
            .read_watcher_state()
            .filter(|s| s.is_fresh())
            .map(|s| s.role)
            .unwrap_or_else(|| "none".into())
    };
    WatcherStatus {
        collector,
        daemon_installed: trove_core::daemon_plist_path().is_some_and(|p| p.exists()),
    }
}

/// One row per known integration for the hub: catalog metadata + enabled
/// flag + permission preflight + when data last landed. Cheap to poll.
#[tauri::command]
#[specta::specta]
async fn integrations_status(
    state: State<'_, AppState>,
) -> Result<Vec<trove_core::IntegrationStatus>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        Ok(vault.integrations_status())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Persist a hub toggle. The collector loops re-read settings every pass, so
/// this takes effect within ~one poll in both the app and troved.
#[tauri::command]
#[specta::specta]
async fn set_integration_enabled(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<Vec<trove_core::IntegrationStatus>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .set_integration_enabled(&id, enabled)
            .map_err(|e| e.to_string())?;
        Ok(vault.integrations_status())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run one connect method of one registered connection — the generic
/// replacement for the per-service connect commands. Blocking thread: OAuth
/// methods open the browser and wait on the loopback redirect (up to 5
/// minutes). OAuth params: optional `client_id`/`client_secret` (BYO
/// credentials, saved on first use); token-paste params: `token`.
#[tauri::command]
#[specta::specta]
async fn connect_run(
    state: State<'_, AppState>,
    connection: String,
    method: String,
    params: std::collections::BTreeMap<String, String>,
) -> Result<trove_core::ConnectStatus, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .connect_run(&connection, &method, &params)
            .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Every registered connection's renderable state in one call. Cheap to
/// poll (file reads).
#[tauri::command]
#[specta::specta]
async fn connect_status_all(
    state: State<'_, AppState>,
) -> Result<Vec<trove_core::ConnectionStatusRow>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.connect_status_all().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forget one account of one connection (synced data stays in the vault).
#[tauri::command]
#[specta::specta]
fn connect_disconnect(
    state: State<AppState>,
    connection: String,
    key: String,
) -> Result<trove_core::ConnectStatus, String> {
    let vault = state.vault.lock().unwrap();
    vault.connect_disconnect(&connection, &key).map_err(|e| e.to_string())
}

/// Manual "Sync now" for any integration with a pull hook (network;
/// blocking thread — first pulls can take a while).
#[tauri::command]
#[specta::specta]
async fn integration_pull(
    state: State<'_, AppState>,
    id: String,
) -> Result<trove_core::PullOutcome, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.integration_pull(&id).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Per-collection sync progress (watermarks, backfill cursors, standing
/// error). Collection itself runs in the watcher owner loop (app or troved).
#[tauri::command]
#[specta::specta]
fn oura_sync_info(state: State<AppState>) -> Option<trove_core::OuraSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_oura_sync()
}

/// Per-account Gmail sync progress (backfill cursors, history ids, standing
/// errors). Collection itself runs in the watcher owner loop (app or troved).
#[tauri::command]
#[specta::specta]
fn gmail_sync_info(state: State<AppState>) -> Option<trove_core::GmailSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_gmail_sync()
}

/// Accounts with latest balances, connection state, and sync metadata for
/// the Finance tab and the hub card. Cheap to poll.
#[tauri::command]
#[specta::specta]
async fn finance_overview(state: State<'_, AppState>) -> Result<trove_core::FinanceOverview, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.finance_overview().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Transactions newest-first, all accounts or one, capped at `limit`.
#[tauri::command]
#[specta::specta]
async fn finance_transactions(
    state: State<'_, AppState>,
    account: Option<String>,
    limit: usize,
) -> Result<Vec<trove_core::finance::Transaction>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .finance_transactions(account.as_deref(), limit)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Visit count + per-domain counts over an inclusive date range (YYYY-MM-DD).
#[tauri::command]
#[specta::specta]
async fn browser_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<BrowserSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.browser_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn browser_timeline(state: State<'_, AppState>, date: String) -> Result<Vec<BrowserVisit>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.browser_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// What the browser extension is engaged with *right now* — open spans that
/// haven't closed yet (e.g. a video still playing), read from the live
/// sidecars. Empty when nothing is being watched or no host is connected.
/// Distinct from `browser_timeline`, which only sees closed spans on disk.
#[tauri::command]
#[specta::specta]
fn browser_live(state: State<AppState>) -> Result<Vec<LiveSpan>, String> {
    let vault = state.vault.lock().unwrap();
    vault.read_browser_live().map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn browser_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.browser_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Cursor/sync metadata — `updated` tells the UI when history last synced.
/// Collection itself runs in the watcher owner loop (app or troved).
#[tauri::command]
#[specta::specta]
fn browser_sync_info(state: State<AppState>) -> Option<BrowserSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_browser_sync()
}

/// Whether this process can read Safari's History.db (Full Disk Access).
/// There is no programmatic FDA prompt — the UI deep-links to System
/// Settings. Note the grant is per-binary: troved needs its own.
#[tauri::command]
#[specta::specta]
fn browser_safari_permission() -> bool {
    trove_core::safari_permission_ok()
}

/// All ad records that closed on one local day (YYYY-MM-DD). Collection is
/// the extension's opt-in page observer; this only reads what landed.
#[tauri::command]
#[specta::specta]
async fn ads_timeline(state: State<'_, AppState>, date: String) -> Result<Vec<AdRecord>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.ads_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Totals plus per-network and per-advertiser aggregates over an inclusive
/// date range (YYYY-MM-DD).
#[tauri::command]
#[specta::specta]
async fn ads_summary(state: State<'_, AppState>, from: String, to: String) -> Result<AdsSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.ads_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Ads-seen and ad-viewing-seconds day series over an inclusive range.
#[tauri::command]
#[specta::specta]
async fn ads_daily(state: State<'_, AppState>, from: String, to: String) -> Result<AdsDaily, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.ads_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Play/skip counts, listening time, and top artists over an inclusive
/// date range (YYYY-MM-DD). "Plays" are full plays (Last.fm rule); skips
/// are recorded too but counted separately.
#[tauri::command]
#[specta::specta]
async fn music_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<MusicSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.music_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn music_timeline(state: State<'_, AppState>, date: String) -> Result<Vec<Play>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.music_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn music_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.music_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Unified media stream (music scrobbles + podcast listens + audible web
/// spans): headline numbers and top artists/shows/sites over an inclusive
/// date range (YYYY-MM-DD).
#[tauri::command]
#[specta::specta]
async fn media_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<MediaSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.media_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn media_timeline(state: State<'_, AppState>, date: String) -> Result<Vec<MediaItem>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.media_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn media_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.media_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Calendar headline numbers over an inclusive date range.
#[tauri::command]
#[specta::specta]
async fn calendar_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<trove_core::CalendarSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calendar_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One day's event occurrences, all-day first.
#[tauri::command]
#[specta::specta]
async fn calendar_timeline(
    state: State<'_, AppState>,
    date: String,
) -> Result<Vec<trove_core::CalendarOccurrence>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calendar_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Scheduled hours per day — the trend series.
#[tauri::command]
#[specta::specta]
async fn calendar_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calendar_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Recent change-stream entries (reschedules, cancellations, additions).
#[tauri::command]
#[specta::specta]
async fn calendar_changes(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<trove_core::CalendarChange>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calendar_changes(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sync metadata: last pass, backfill state, standing error.
#[tauri::command]
#[specta::specta]
fn calendar_sync_info(state: State<AppState>) -> Option<trove_core::CalendarSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_calendar_sync()
}

/// EventKit authorization for this process: "granted" / "denied" /
/// "not-determined", for events and reminders respectively.
#[tauri::command]
#[specta::specta]
fn calendar_permission() -> (String, String) {
    (
        trove_core::events_auth_status().as_str().to_string(),
        trove_core::reminders_auth_status().as_str().to_string(),
    )
}

/// Fire the Calendar + Reminders TCC prompts (no-ops once decided). Runs off
/// the main thread; blocks up to a minute each so the user can answer.
/// Returns the post-request (granted-events, granted-reminders) pair.
#[tauri::command]
#[specta::specta]
async fn request_calendar_permission() -> Result<(bool, bool), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let events = trove_core::request_events_access(60);
        let reminders = trove_core::request_reminders_access(60);
        (events, reminders)
    })
    .await
    .map_err(|e| e.to_string())
}

/// The newest weather observation on file (the "now" card).
#[tauri::command]
#[specta::specta]
async fn weather_latest(
    state: State<'_, AppState>,
) -> Result<Option<trove_core::WeatherObservation>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.weather_latest().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One day's hourly observations.
#[tauri::command]
#[specta::specta]
async fn weather_timeline(
    state: State<'_, AppState>,
    date: String,
) -> Result<Vec<trove_core::WeatherObservation>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.weather_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Per-day aggregates (temps, precipitation, wind, UV) over an inclusive
/// range; uncovered days are omitted.
#[tauri::command]
#[specta::specta]
async fn weather_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<trove_core::WeatherDay>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.weather_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sync metadata: last pass, last used location, standing error.
#[tauri::command]
#[specta::specta]
fn weather_sync_info(state: State<AppState>) -> Option<trove_core::WeatherSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_weather_sync()
}

/// The manual location, if set.
#[tauri::command]
#[specta::specta]
fn weather_location(state: State<AppState>) -> Option<trove_core::WeatherLocation> {
    let vault = state.vault.lock().unwrap();
    vault.weather_location()
}

/// Set or clear (`null`) the manual weather location.
#[tauri::command]
#[specta::specta]
fn set_weather_location(
    state: State<AppState>,
    location: Option<trove_core::WeatherLocation>,
) -> Result<(), String> {
    let vault = state.vault.lock().unwrap();
    vault.set_weather_location(location).map_err(|e| e.to_string())
}

/// Location Services authorization for this process.
#[tauri::command]
#[specta::specta]
fn weather_permission() -> String {
    trove_core::corelocation::auth_status().as_str().to_string()
}

/// Fire the Location Services TCC prompt (no-op once decided). Runs off the
/// main thread; blocks up to a minute so the user can answer.
#[tauri::command]
#[specta::specta]
async fn request_weather_permission() -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(|| trove_core::corelocation::request_access(60))
        .await
        .map_err(|e| e.to_string())
}

/// Headline task numbers (open/due/overdue/completed-7d + per-project
/// counts), across every source under tasks/.
#[tauri::command]
#[specta::specta]
async fn tasks_overview(state: State<'_, AppState>) -> Result<TasksOverview, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.tasks_overview().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// All open tasks across every source.
#[tauri::command]
#[specta::specta]
async fn tasks_list(state: State<'_, AppState>) -> Result<Vec<Task>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.tasks_list().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Completions per day over an inclusive date range (YYYY-MM-DD), from the
/// append-only event stream.
#[tauri::command]
#[specta::specta]
async fn tasks_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.tasks_completed_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sync metadata — when each source last synced and any standing error
/// (e.g. an expired token). Collection itself runs in the watcher owner
/// loop (app or troved).
#[tauri::command]
#[specta::specta]
fn tasks_sync_info(state: State<AppState>) -> Option<TasksSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_tasks_sync()
}

/// Message counts + per-conversation volume over an inclusive date range.
#[tauri::command]
#[specta::specta]
async fn correspondence_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<trove_core::CorrespondenceSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.correspondence_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn correspondence_timeline(
    state: State<'_, AppState>,
    date: String,
) -> Result<Vec<trove_core::Message>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.correspondence_timeline(&date).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn correspondence_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.correspondence_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One page of the Email browser: ranged, newest-first, optionally filtered
/// to one account and/or a substring query, with whole records (body text
/// included) plus the totals and per-account counts the filter chips need.
#[tauri::command]
#[specta::specta]
async fn email_list(
    state: State<'_, AppState>,
    from: String,
    to: String,
    account: Option<String>,
    query: Option<String>,
    limit: u32,
    offset: u32,
) -> Result<trove_core::EmailPage, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault
            .email_list(&from, &to, account.as_deref(), query.as_deref(), limit, offset)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether this process can read the Messages database (Full Disk Access —
/// same per-binary, deep-link-only grant as Safari history).
#[tauri::command]
#[specta::specta]
fn imessage_permission() -> bool {
    trove_core::imessage_permission_ok()
}

/// Cursor/sync metadata — `updated` tells the UI when messages last synced.
/// Collection itself runs in the watcher owner loop (app or troved).
#[tauri::command]
#[specta::specta]
fn imessage_sync_info(state: State<AppState>) -> Option<trove_core::IMessageSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_imessage_sync()
}

/// Call counts, talk time, and per-caller volume over an inclusive range.
#[tauri::command]
#[specta::specta]
async fn calls_summary(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<trove_core::CallsSummary, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calls_summary(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
async fn calls_daily(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<Vec<SeriesPoint>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calls_daily(&from, &to).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The newest calls of a range, most recent first — the call log list.
#[tauri::command]
#[specta::specta]
async fn calls_recent(
    state: State<'_, AppState>,
    from: String,
    to: String,
    limit: usize,
) -> Result<Vec<trove_core::Message>, String> {
    let root = state.vault.lock().unwrap().root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let vault = Vault::open_or_create(root).map_err(|e| e.to_string())?;
        vault.calls_recent(&from, &to, limit).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether this process can read the call history database (Full Disk
/// Access — same per-binary, deep-link-only grant as Messages).
#[tauri::command]
#[specta::specta]
fn calls_permission() -> bool {
    trove_core::calls_permission_ok()
}

#[tauri::command]
#[specta::specta]
fn calls_sync_info(state: State<AppState>) -> Option<trove_core::CallsSyncState> {
    let vault = state.vault.lock().unwrap();
    vault.read_calls_sync()
}

/// Whether Screen Recording is granted (needed for window titles).
#[tauri::command]
#[specta::specta]
fn activity_permission() -> bool {
    trove_core::screen_recording_ok()
}

/// Prompt for Screen Recording (first call only — afterwards macOS requires a
/// trip to System Settings). Returns the resulting permission state.
#[tauri::command]
#[specta::specta]
fn request_activity_permission() -> bool {
    trove_core::request_screen_recording()
}

/// The tauri-specta builder: the single registry of every IPC command, used
/// by `run()` for the invoke handler and by the bindings export (debug runs
/// and the `export_typescript_bindings` test) to generate `src/bindings.ts`.
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new().commands(tauri_specta::collect_commands![
        vault_info,
        list_artifacts,
        read_artifact,
        write_artifact,
        create_artifact,
        delete_artifact,
        import_artifacts,
        search_artifacts,
        run_import,
        vault_manifest,
        read_stream,
        normalizer_detect,
        normalizer_suggest_payload,
        normalizer_advisor_status,
        normalizer_suggest,
        normalizer_preview,
        normalizer_confirm,
        normalizer_reproject,
        normalizer_list_mappings,
        normalizer_delete_mapping,
        normalizer_decline,
        normalizer_list_declined,
        normalizer_read_raw,
        normalizer_save_key,
        normalizer_delete_key,
        list_health_metrics,
        health_series,
        health_metrics_unified,
        health_series_unified,
        oura_overview,
        oura_sleep_nights,
        oura_heartrate_range,
        health_workouts,
        activity_summary,
        activity_timeline,
        activity_daily,
        activity_current,
        activity_permission,
        request_activity_permission,
        watcher_status,
        screen_time_summary,
        screen_time_timeline,
        screen_time_daily,
        screen_time_devices,
        screen_time_permission,
        browser_summary,
        browser_timeline,
        browser_live,
        browser_daily,
        browser_sync_info,
        browser_safari_permission,
        ads_timeline,
        ads_summary,
        ads_daily,
        music_summary,
        music_timeline,
        music_daily,
        media_summary,
        media_timeline,
        media_daily,
        tasks_overview,
        tasks_list,
        tasks_daily,
        tasks_sync_info,
        calendar_summary,
        calendar_timeline,
        calendar_daily,
        calendar_changes,
        calendar_sync_info,
        calendar_permission,
        request_calendar_permission,
        correspondence_summary,
        correspondence_timeline,
        correspondence_daily,
        email_list,
        imessage_permission,
        imessage_sync_info,
        calls_summary,
        calls_daily,
        calls_recent,
        calls_permission,
        calls_sync_info,
        weather_latest,
        weather_timeline,
        weather_daily,
        weather_sync_info,
        weather_location,
        set_weather_location,
        weather_permission,
        request_weather_permission,
        connect_run,
        connect_status_all,
        connect_disconnect,
        integration_pull,
        oura_sync_info,
        gmail_sync_info,
        integrations_status,
        set_integration_enabled,
        finance_overview,
        finance_transactions
    ])
}

/// Export `src/bindings.ts` from the builder (debug runs + the bindings test).
#[cfg(any(debug_assertions, test))]
fn export_bindings(builder: &tauri_specta::Builder<tauri::Wry>) {
    builder
        .export(
            // u64/usize cross the boundary as plain JSON numbers today (serde_json
            // serializes them as numbers), so export them as `number`, not bigint.
            // @ts-nocheck because the generated runtime keeps event/channel
            // helpers around even when unused (tsc noUnusedLocals).
            specta_typescript::Typescript::default()
                .header("// @ts-nocheck")
                .bigint(specta_typescript::BigIntExportBehavior::Number),
            "../src/bindings.ts",
        )
        .expect("failed to export typescript bindings");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let vault = Vault::open_or_create(Vault::default_root())
        .expect("failed to open or create vault at ~/Documents/Trove");

    // Contend for the vault's single-writer lock on a background thread: this
    // app collects only while it holds the lock, defers to troved (or another
    // app instance) otherwise, and takes over automatically if the owner
    // exits. Uses its own `Vault` handle so it never contends with command
    // handlers on the `AppState` mutex.
    let watch = WatchControl::new();
    let watcher_thread = {
        let root = vault.root().to_path_buf();
        let control = watch.clone();
        std::thread::spawn(move || {
            if let Err(e) = trove_core::run_watcher(root, WatcherRole::App, control) {
                eprintln!("activity watcher stopped: {e:#}");
            }
        })
    };
    let watcher_thread = Mutex::new(Some(watcher_thread));
    let watch_for_exit = watch.clone();

    let builder = specta_builder();

    // Keep src/bindings.ts current during development (CI checks for drift
    // via scripts/check-bindings.sh).
    #[cfg(debug_assertions)]
    export_bindings(&builder);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            vault: Mutex::new(vault),
            watch,
        })
        .invoke_handler(builder.invoke_handler())
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_app, event| {
            // Flush the in-progress event on quit (it would otherwise be
            // lost) and release the lock so troved can take over.
            if let tauri::RunEvent::Exit = event {
                watch_for_exit.stop();
                if let Some(h) = watcher_thread.lock().unwrap().take() {
                    let _ = h.join();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    /// Regenerates ../src/bindings.ts — the way to refresh bindings without
    /// launching the GUI (`scripts/check-bindings.sh` runs this in CI).
    #[test]
    fn export_typescript_bindings() {
        super::export_bindings(&super::specta_builder());
    }
}
