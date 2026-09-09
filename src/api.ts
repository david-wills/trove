// Thin façade over the generated tauri-specta bindings (src/bindings.ts).
//
// All IPC types are generated from the Rust command signatures — run
// `cargo test -p trove-app export_typescript_bindings` (or any debug run of
// the app) to regenerate, and `scripts/check-bindings.sh` to verify drift.
// This module keeps the app's historical surface: the same `api.*` function
// names/signatures, throw-on-error semantics, and type names.

import { commands } from "./bindings";
import type * as B from "./bindings";
import type { Result } from "./bindings";

export type {
  ActivityEvent,
  ActivitySummary,
  AdRecord,
  AdsDaily,
  AdsSummary,
  AdUsage,
  AppUsage,
  ArtifactMeta,
  AttachmentMeta,
  BrowserSummary,
  CalendarChange,
  CalendarCount,
  CalendarSummary,
  CalendarSyncState,
  CallerUsage,
  CallsSummary,
  CallsSyncState,
  ChatUsage,
  DomainUsage,
  FieldChange,
  FinanceOverview,
  FinanceSyncState,
  HeartratePoint,
  IMessageSyncState,
  ImportInfo,
  ImportOutcome,
  ImportParam,
  IntegrationKind,
  IntegrationStatus,
  JsonValue,
  Manifest,
  ManifestDomain,
  MediaItem,
  MediaUsage,
  MetricKind,
  MetricSummary,
  MusicSummary,
  OuraCollectionState,
  OuraDayScore,
  PermissionInfo,
  Play,
  ProjectCount,
  ScreenTimeAppUsage,
  ScreenTimeDeviceUsage,
  ScreenTimeSession,
  ScreenTimeSummary,
  SeriesPoint,
  SleepNight,
  SourceSync,
  StreamPage,
  Subtask,
  TasksOverview,
  VaultInfo,
  WatcherStatus,
  WeatherDay,
} from "./bindings";

// Frontend-only types: these never cross IPC in a command signature, so the
// generator doesn't know about them.
export type Bucket = "day" | "week" | "month";
export type HealthSource = "apple-health" | "oura";

/** Payload of the "import-progress" event, emitted while any `runImport`
 *  runs — filter by `integration_id`. */
export interface ImportProgress {
  integration_id: string;
  records: number;
  percent: number;
}

// ---------------------------------------------------------------------------
// Normalizer (R2) — the rich mapping/detection types cross IPC as JsonValue
// (an untagged enum + arbitrary cells don't derive specta::Type), typed here
// to match the Rust serde shape exactly. See `crates/trove-core/src/normalizer.rs`.
// ---------------------------------------------------------------------------

/** Field-level contract metadata surfaced in the binding editor. */
export interface FieldMeta {
  name: string;
  description?: string;
  examples?: B.JsonValue[];
  type_?: string;
  required: boolean;
}
export interface Signature {
  headers: string[];
  format: string;
}
export interface Provenance {
  created?: string;
  suggested_by?: string;
}
/** exactly one of `column` | `hash` (serde untagged). */
export type GuidRecipe =
  | { column: string; prefix?: string }
  | { hash: string[]; algo?: string; prefix?: string };
export interface Binding {
  from: string;
  to: string;
  coerce?: string | null;
  with?: B.JsonValue;
}
/** A confirmed/draft source→contract mapping (`.trove/mappings/<source>.json`). */
export interface Mapping {
  version: number;
  source: string;
  domain: string;
  shape: string;
  signature: Signature;
  bindings: Binding[];
  constants?: Record<string, B.JsonValue>;
  guid: GuidRecipe;
  unbound: string;
  provenance?: Provenance;
}
export interface ContractCandidate {
  domain: string;
  shape: string;
  score: number;
  draft: Mapping;
  unbound_headers: string[];
}
/** The three fates a dropped file resolves to (serde tag `kind`, kebab-case). */
export type DetectOutcome =
  | { kind: "route"; integration_id: string; name: string; shape_label: string }
  | { kind: "contract"; confident: boolean; candidates: ContractCandidate[] }
  | { kind: "no-match" };
export interface Detection {
  headers: string[];
  sample_rows: Record<string, B.JsonValue>[];
  format: string;
  outcome: DetectOutcome;
}
export interface SuggestCandidateMeta {
  domain: string;
  shape: string;
  guid_field: string;
  required: string[];
  fields: FieldMeta[];
}
/** The literal data that would leave the machine for an LLM suggestion. */
export interface SuggestPayload {
  headers: string[];
  sample_rows: Record<string, B.JsonValue>[];
  format: string;
  candidates: SuggestCandidateMeta[];
}
export interface SuggestPayloadResponse {
  payload: SuggestPayload;
  /** The exact Anthropic request body (headers are added out-of-band). */
  request_body: B.JsonValue;
}
export interface LlmAdvisorStatus {
  available: boolean;
  /** "byo" | "baked" when available. */
  source?: string;
  reason?: string;
  model: string;
}
export interface DeclinedDrop {
  source: string;
  /** Vault-relative path of the landed raw file. */
  file: string;
  landed: string;
}
export interface RawPage {
  headers: string[];
  rows: Record<string, B.JsonValue>[];
  format: string;
  offset: number;
  has_more: boolean;
}
export interface PreviewResult {
  total: number;
  valid: number;
  invalid: number;
  invalid_samples: { line: number; reason: string }[];
  sample_rows: Record<string, B.JsonValue>[];
}

// ---------------------------------------------------------------------------
// Read-direction refinements of the generated types.
//
// specta marks every `#[serde(default)]` field optional and renders maps as
// `Partial<Record<…>>` — correct for *deserialization*, but these commands
// only ever serialize Rust → frontend, where serde always writes the fields
// and map values are never undefined. The aliases below restore the actual
// wire shape; the api functions cast their results accordingly. Runtime
// behavior is untouched.
// ---------------------------------------------------------------------------

/** Make serde(default) fields (always serialized) required again. */
type AlwaysSerialized<T, K extends keyof T> = Omit<T, K> & {
  [P in K]-?: T[P];
};

// Historical names for generated types whose Rust names differ.
export type FinanceAccount = B.AccountOverview;
export type FinanceTransaction = AlwaysSerialized<
  B.Transaction,
  "currency" | "pending"
>;
export type ScreenTimeDevice = AlwaysSerialized<B.DeviceInfo, "last_seen">;
export type WeatherLocation = AlwaysSerialized<B.WeatherLocation, "place">;

export type UnifiedSourceInfo = Omit<B.UnifiedSourceInfo, "source"> & {
  source: HealthSource;
};
export type UnifiedMetric = Omit<B.UnifiedMetric, "sources"> & {
  sources: UnifiedSourceInfo[];
};
export type SourceSeries = Omit<B.SourceSeries, "source"> & {
  source: HealthSource;
};
export type WorkoutItem = Omit<B.WorkoutItem, "source"> & {
  source: HealthSource;
};

export type Task = AlwaysSerialized<
  B.Task,
  "project" | "notes" | "status" | "priority" | "all_day"
>;
export type Message = AlwaysSerialized<
  B.Message,
  "chat" | "sender" | "from_me" | "kind" | "text"
>;
export type BrowserVisit = AlwaysSerialized<
  B.BrowserVisit,
  "title" | "profile" | "duration_secs" | "source"
>;
export type LiveSpan = AlwaysSerialized<
  B.LiveSpan,
  "foreground_secs" | "audible" | "foreground"
>;
export type CalendarOccurrence = AlwaysSerialized<
  B.CalendarOccurrence,
  | "occurrence"
  | "start"
  | "end"
  | "all_day"
  | "title"
  | "calendar"
  | "account"
  | "recurring"
>;
export type WeatherObservation = AlwaysSerialized<
  B.WeatherObservation,
  | "lat"
  | "lon"
  | "temp_c"
  | "apparent_c"
  | "humidity_pct"
  | "dew_point_c"
  | "precip_mm"
  | "rain_mm"
  | "showers_mm"
  | "snowfall_cm"
  | "weather_code"
  | "cloud_pct"
  | "pressure_hpa"
  | "wind_kmh"
  | "wind_dir_deg"
  | "wind_gusts_kmh"
  | "is_day"
  | "source"
>;
export type WeatherSyncState = AlwaysSerialized<
  B.WeatherSyncState,
  "updated" | "last_observation" | "lat" | "lon" | "place"
>;
export type GmailAccountState = AlwaysSerialized<
  B.GmailAccountState,
  "email" | "backfill_started" | "backfill_done" | "messages"
>;
export type GmailSyncState = Omit<B.GmailSyncState, "accounts"> & {
  accounts: Record<string, GmailAccountState>;
};
export type OuraSyncState = Omit<B.OuraSyncState, "collections"> & {
  collections: Record<string, B.OuraCollectionState>;
};
export type TasksSyncState = Omit<B.TasksSyncState, "sources"> & {
  sources: Record<string, B.SourceSync>;
};
export type MediaSummary = Omit<B.MediaSummary, "sources" | "devices"> & {
  sources: Record<string, number>;
  devices: Record<string, number>;
};
export type CorrespondenceSummary = Omit<B.CorrespondenceSummary, "sources"> & {
  sources: Record<string, number>;
};
export type BrowserSyncState = Omit<B.BrowserSyncState, "cursors"> & {
  cursors: Record<string, number>;
};
export type EmailPage = Omit<B.EmailPage, "messages" | "accounts"> & {
  messages: Message[];
  accounts: Record<string, number>;
};

/** Restore throw-on-error semantics on top of tauri-specta's Result shape. */
async function unwrap<T>(result: Promise<Result<T, string>>): Promise<T> {
  const r = await result;
  if (r.status === "error") throw r.error;
  return r.data;
}

export const api = {
  vaultInfo: () => unwrap(commands.vaultInfo()),
  listArtifacts: () => unwrap(commands.listArtifacts()),
  readArtifact: (path: string) => unwrap(commands.readArtifact(path)),
  writeArtifact: (path: string, content: string) =>
    unwrap(commands.writeArtifact(path, content)),
  createArtifact: (title: string) => unwrap(commands.createArtifact(title)),
  deleteArtifact: (path: string) => unwrap(commands.deleteArtifact(path)),
  /** Copy dropped files (absolute OS paths, .md/.txt only) into the vault. */
  importArtifacts: (paths: string[]) => unwrap(commands.importArtifacts(paths)),
  searchArtifacts: (query: string) => unwrap(commands.searchArtifacts(query)),
  listHealthMetrics: () => unwrap(commands.listHealthMetrics()),
  healthSeries: (metric: string, bucket: Bucket) =>
    unwrap(commands.healthSeries(metric, bucket)),
  healthMetricsUnified: () =>
    unwrap(commands.healthMetricsUnified()) as Promise<UnifiedMetric[]>,
  healthSeriesUnified: (metric: string, bucket: Bucket) =>
    unwrap(commands.healthSeriesUnified(metric, bucket)) as Promise<
      SourceSeries[]
    >,
  ouraOverview: () => unwrap(commands.ouraOverview()),
  ouraSleepNights: (limit: number) => unwrap(commands.ouraSleepNights(limit)),
  ouraHeartrateRange: (start: string, end: string, maxPoints: number) =>
    unwrap(commands.ouraHeartrateRange(start, end, maxPoints)),
  healthWorkouts: (limit: number) =>
    unwrap(commands.healthWorkouts(limit)) as Promise<WorkoutItem[]>,
  activitySummary: (from: string, to: string) =>
    unwrap(commands.activitySummary(from, to)),
  activityTimeline: (date: string) => unwrap(commands.activityTimeline(date)),
  activityDaily: (from: string, to: string) =>
    unwrap(commands.activityDaily(from, to)),
  activityCurrent: () => commands.activityCurrent(),
  activityPermission: () => commands.activityPermission(),
  requestActivityPermission: () => commands.requestActivityPermission(),
  watcherStatus: () => commands.watcherStatus(),
  screenTimeSummary: (
    from: string,
    to: string,
    device: string | null,
    includeIdle: boolean
  ) => unwrap(commands.screenTimeSummary(from, to, device, includeIdle)),
  screenTimeTimeline: (
    date: string,
    device: string | null,
    includeIdle: boolean
  ) => unwrap(commands.screenTimeTimeline(date, device, includeIdle)),
  screenTimeDaily: (
    from: string,
    to: string,
    device: string | null,
    includeIdle: boolean
  ) => unwrap(commands.screenTimeDaily(from, to, device, includeIdle)),
  // BTreeMap values are always present — undo specta's `Partial` map keys.
  screenTimeDevices: () =>
    commands.screenTimeDevices() as Promise<Record<string, ScreenTimeDevice>>,
  screenTimePermission: () => commands.screenTimePermission(),
  /** Run one connect method of one registered connection. OAuth params:
   *  optional client_id/client_secret (BYO credentials); token-paste
   *  params: token. */
  connectRun: (connection: string, method: string, params: Record<string, string>) =>
    unwrap(commands.connectRun(connection, method, params)),
  /** Every registered connection's renderable state. Cheap to poll. */
  connectStatusAll: () => unwrap(commands.connectStatusAll()),
  /** Forget one account of one connection (its data stays). */
  connectDisconnect: (connection: string, key: string) =>
    unwrap(commands.connectDisconnect(connection, key)),
  /** Manual "Sync now" for any integration with a pull hook. */
  integrationPull: (id: string) => unwrap(commands.integrationPull(id)),
  ouraSyncInfo: () =>
    commands.ouraSyncInfo() as Promise<OuraSyncState | null>,
  /** Per-account Gmail sync progress, or null before the first sync. */
  gmailSyncInfo: () =>
    commands.gmailSyncInfo() as Promise<GmailSyncState | null>,
  browserSummary: (from: string, to: string) =>
    unwrap(commands.browserSummary(from, to)),
  browserTimeline: (date: string) =>
    unwrap(commands.browserTimeline(date)) as Promise<BrowserVisit[]>,
  browserLive: () => unwrap(commands.browserLive()) as Promise<LiveSpan[]>,
  browserDaily: (from: string, to: string) =>
    unwrap(commands.browserDaily(from, to)),
  browserSyncInfo: () =>
    commands.browserSyncInfo() as Promise<BrowserSyncState | null>,
  browserSafariPermission: () => commands.browserSafariPermission(),
  adsTimeline: (date: string) => unwrap(commands.adsTimeline(date)),
  adsSummary: (from: string, to: string) =>
    unwrap(commands.adsSummary(from, to)),
  adsDaily: (from: string, to: string) => unwrap(commands.adsDaily(from, to)),
  mediaSummary: (from: string, to: string) =>
    unwrap(commands.mediaSummary(from, to)) as Promise<MediaSummary>,
  mediaTimeline: (date: string) => unwrap(commands.mediaTimeline(date)),
  mediaDaily: (from: string, to: string) =>
    unwrap(commands.mediaDaily(from, to)),
  tasksOverview: () => unwrap(commands.tasksOverview()),
  tasksList: () => unwrap(commands.tasksList()) as Promise<Task[]>,
  tasksDaily: (from: string, to: string) =>
    unwrap(commands.tasksDaily(from, to)),
  tasksSyncInfo: () =>
    commands.tasksSyncInfo() as Promise<TasksSyncState | null>,
  calendarSummary: (from: string, to: string) =>
    unwrap(commands.calendarSummary(from, to)),
  calendarTimeline: (date: string) =>
    unwrap(commands.calendarTimeline(date)) as Promise<CalendarOccurrence[]>,
  calendarDaily: (from: string, to: string) =>
    unwrap(commands.calendarDaily(from, to)),
  calendarChanges: (from: string, to: string) =>
    unwrap(commands.calendarChanges(from, to)),
  calendarSyncInfo: () => commands.calendarSyncInfo(),
  /** [events, reminders] — "granted" | "denied" | "not-determined". */
  calendarPermission: () => commands.calendarPermission(),
  /** Fires both TCC prompts; resolves with [events, reminders] grants. */
  requestCalendarPermission: () => unwrap(commands.requestCalendarPermission()),
  correspondenceSummary: (from: string, to: string) =>
    unwrap(commands.correspondenceSummary(from, to)) as Promise<
      CorrespondenceSummary
    >,
  correspondenceTimeline: (date: string) =>
    unwrap(commands.correspondenceTimeline(date)) as Promise<Message[]>,
  correspondenceDaily: (from: string, to: string) =>
    unwrap(commands.correspondenceDaily(from, to)),
  emailList: (
    from: string,
    to: string,
    account: string | null,
    query: string | null,
    limit: number,
    offset: number
  ) =>
    unwrap(
      commands.emailList(from, to, account, query, limit, offset)
    ) as Promise<EmailPage>,
  imessagePermission: () => commands.imessagePermission(),
  imessageSyncInfo: () => commands.imessageSyncInfo(),
  callsSummary: (from: string, to: string) =>
    unwrap(commands.callsSummary(from, to)),
  callsDaily: (from: string, to: string) =>
    unwrap(commands.callsDaily(from, to)),
  callsRecent: (from: string, to: string, limit: number) =>
    unwrap(commands.callsRecent(from, to, limit)) as Promise<Message[]>,
  callsPermission: () => commands.callsPermission(),
  callsSyncInfo: () => commands.callsSyncInfo(),
  weatherLatest: () =>
    unwrap(commands.weatherLatest()) as Promise<WeatherObservation | null>,
  weatherTimeline: (date: string) =>
    unwrap(commands.weatherTimeline(date)) as Promise<WeatherObservation[]>,
  weatherDaily: (from: string, to: string) =>
    unwrap(commands.weatherDaily(from, to)),
  weatherSyncInfo: () =>
    commands.weatherSyncInfo() as Promise<WeatherSyncState | null>,
  weatherLocation: () =>
    commands.weatherLocation() as Promise<WeatherLocation | null>,
  /** Pass null to clear the manual location. */
  setWeatherLocation: (location: WeatherLocation | null) =>
    unwrap(commands.setWeatherLocation(location)),
  /** "granted" | "denied" | "not-determined" for this app's process. */
  weatherPermission: () => commands.weatherPermission(),
  /** Fires the Location Services TCC prompt; resolves with the grant. */
  requestWeatherPermission: () => unwrap(commands.requestWeatherPermission()),
  financeOverview: () => unwrap(commands.financeOverview()),
  financeTransactions: (account: string | null, limit: number) =>
    unwrap(commands.financeTransactions(account, limit)) as Promise<
      FinanceTransaction[]
    >,
  /** Run any import-kind integration on a picked file; param keys come from
   *  the integration's `ImportInfo.params`. Progress streams on the
   *  "import-progress" event (see `ImportProgress`). */
  runImport: (integrationId: string, path: string, params: Record<string, string>) =>
    unwrap(commands.runImport(integrationId, path, params)),
  integrationsStatus: () => unwrap(commands.integrationsStatus()),
  setIntegrationEnabled: (id: string, enabled: boolean) =>
    unwrap(commands.setIntegrationEnabled(id, enabled)),
  /** The rebuildable index of every data folder in the vault. */
  vaultManifest: () => unwrap(commands.vaultManifest()),
  /** Newest-first raw records from any date-partitioned JSONL folder. */
  readStream: (dir: string, limit: number, offset: number) =>
    unwrap(commands.readStream(dir, limit, offset)),

  // --- Normalizer (R2) — drop a file, map it to a contract, keep raw. ---
  /** Detect a dropped file's fate (route / contract candidates / no-match). */
  normalizerDetect: (path: string) =>
    unwrap(commands.normalizerDetect(path)) as unknown as Promise<Detection>,
  /** The literal LLM consent payload + request body for this file. */
  normalizerSuggestPayload: (path: string) =>
    unwrap(commands.normalizerSuggestPayload(path)) as unknown as Promise<
      SuggestPayloadResponse
    >,
  /** Whether the LLM advisor is usable, with a reason when not. No network. */
  normalizerAdvisorStatus: () =>
    unwrap(commands.normalizerAdvisorStatus()) as unknown as Promise<
      LlmAdvisorStatus
    >,
  /** Run the consented advisor; returns a draft mapping pre-filling the form. */
  normalizerSuggest: (source: string, path: string) =>
    unwrap(commands.normalizerSuggest(source, path)) as unknown as Promise<
      Mapping
    >,
  /** Dry-run a mapping against the file (no write): counts + a sample. */
  normalizerPreview: (path: string, mapping: Mapping) =>
    unwrap(
      commands.normalizerPreview(path, mapping as unknown as B.JsonValue)
    ) as unknown as Promise<PreviewResult>,
  /** Persist the mapping, land raw, and project the rows. Progress on the
   *  "import-progress" event tagged `normalizer`. */
  normalizerConfirm: (path: string, mapping: Mapping) =>
    unwrap(commands.normalizerConfirm(path, mapping as unknown as B.JsonValue)),
  /** Re-project a source from raw after its mapping was edited. */
  normalizerReproject: (source: string) =>
    unwrap(commands.normalizerReproject(source)),
  /** Every stored mapping, in source order. */
  normalizerListMappings: () =>
    unwrap(commands.normalizerListMappings()) as unknown as Promise<Mapping[]>,
  /** Delete a mapping and its projection, keeping raw. */
  normalizerDeleteMapping: (source: string) =>
    unwrap(commands.normalizerDeleteMapping(source)),
  /** Land a nothing-fits drop raw under `imports/<source>/`. */
  normalizerDecline: (source: string, path: string) =>
    unwrap(commands.normalizerDecline(source, path)) as unknown as Promise<
      DeclinedDrop
    >,
  /** Every declined raw drop under `imports/`. */
  normalizerListDeclined: () =>
    unwrap(commands.normalizerListDeclined()) as unknown as Promise<
      DeclinedDrop[]
    >,
  /** A page of a raw file (vault-relative path) as a table. O(offset+limit). */
  normalizerReadRaw: (path: string, offset: number, limit: number) =>
    unwrap(commands.normalizerReadRaw(path, offset, limit)) as unknown as Promise<
      RawPage
    >,
  /** Save a BYO Anthropic key for the advisor (0600). */
  normalizerSaveKey: (key: string) => unwrap(commands.normalizerSaveKey(key)),
  /** Forget the saved BYO advisor key. */
  normalizerDeleteKey: () => unwrap(commands.normalizerDeleteKey()),
};
