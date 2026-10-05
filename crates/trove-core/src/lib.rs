//! trove-core: the vault.
//!
//! A Trove vault is a plain folder of open-format files (markdown, CSV,
//! JSONL). This crate owns the vault layout and all reads/writes to it, so
//! the GUI app and the background collector daemon share one implementation
//! and the files stay the single source of truth.

pub mod activity;
pub mod ads;
pub mod apple_mail;
pub mod boards;
pub mod books;
pub mod browser;
pub mod browser_ext;
pub mod browser_searches;
pub mod calendar;
pub mod calls;
pub mod cloud_folder;
pub mod columns;
pub mod connect;
pub mod contacts;
pub mod contracts;
pub mod corelocation;
pub mod correspondence;
pub mod dropbox;
pub mod email;
pub mod environment;
pub mod eventkit;
pub mod fathom;
pub mod finance;
pub mod gmail;
pub mod google_books;
pub mod google_calendar;
pub mod google_contacts;
pub mod google_drive;
pub mod google_tasks;
pub mod habits;
pub mod health;
pub mod health_medical;
pub mod health_nutrition;
pub mod health_sleep;
pub mod health_unified;
pub mod home;
pub mod icloud_drive;
pub mod imessage;
pub mod integrations;
pub mod letterboxd;
pub mod location;
pub mod macos_downloads;
pub mod macos_screenshots;
pub mod media;
pub mod meetings;
pub mod meta_encoding;
pub mod music;
pub mod music_library;
pub mod normalizer;
pub mod notes;
pub mod oura;
pub mod photos;
pub mod podcasts;
pub mod query;
pub mod reading;
pub mod registry;
pub mod runner;
pub mod screen_time;
pub mod segb;
pub mod slack;
pub mod social;
pub mod store;
pub mod sync;
pub mod tasks;
pub mod time_entries;
pub mod travel;
pub mod vault;
pub mod voice;
pub mod weather;
pub mod youtube;

pub use activity::{ActivityEvent, ActivitySummary, AppUsage};
pub use ads::{AdRecord, AdUsage, AdsDaily, AdsSummary};
pub use books::{
    diff_annotations, diff_assets, read_books_via_copy, BookAnnotation, BookAsset, BookEvent,
    BooksSnapshotStats,
};
pub use browser::{
    safari_permission_ok, BrowserSummary, BrowserSyncState, BrowserSyncStats, BrowserVisit,
    DomainUsage, BROWSER_SYNC_SECS,
};
pub use browser_ext::{LiveSpan, LiveState};
pub use browser_searches::Search;
pub use calendar::{
    diff_occurrences, CalendarChange, CalendarCount, CalendarOccurrence, CalendarSummary,
    CalendarSyncState, CalendarSyncStats,
};
pub use calls::{
    calls_permission_ok, CallerUsage, CallsSummary, CallsSyncState, CallsSyncStats,
};
pub use contacts::{normalize_email, normalize_phone, Contact, ContactOrg};
pub use contracts::{ContractKind, DomainContract, Manifest, ManifestDomain, DOMAINS};
pub use correspondence::{
    AttachmentMeta, ChatUsage, CorrespondenceSummary, EmailPage, Message,
};
pub use environment::{Almanac, EnvGeoEvent, EnvReading};
pub use email::EmailImportStats;
pub use gmail::{GmailAccountState, GmailSyncState, GmailSyncStats, GMAIL_SYNC_SECS};
pub use eventkit::{
    events_auth_status, reminders_auth_status, request_events_access, request_reminders_access,
    AuthStatus,
};
pub use finance::{
    AccountOverview, BalanceSnapshot, FinanceOverview, FinanceSyncState, FinanceSyncStats, LineItem,
};
pub use finance::import::CsvImportStats;
pub use habits::{Checkin, Habit};
pub use health::{Bucket, HealthSummary, ImportProgress, MetricKind, MetricSummary, SeriesPoint};
pub use health_medical::Observation;
pub use health_nutrition::Entry as NutritionEntry;
pub use health_sleep::{apple_sessions, dedupe_relays, oura_session, AppleInterval, Session as SleepSession};
pub use health_unified::{
    HeartratePoint, OuraDayScore, SleepNight, SourceSeries, UnifiedMetric, UnifiedSourceInfo,
    WorkoutItem,
};
pub use home::HomeReading;
pub use imessage::{
    imessage_permission_ok, IMessageSyncState, IMessageSyncStats, IMESSAGE_SYNC_SECS,
};
pub use integrations::{
    Integration, IntegrationKind, IntegrationSettings, IntegrationStatus, PermissionInfo,
    CONNECTIONS, INTEGRATIONS,
};
pub use media::{MediaItem, MediaSummary, MediaUsage};
pub use meetings::Meeting;
pub use notes::Note;
pub use music::{ArtistUsage, MusicSummary, Play};
pub use music_library::{
    diff_snapshots, read_library, should_snapshot, FieldChange, LibraryEvent,
    LibrarySnapshotStats, LibraryTrack,
};
pub use oura::{OuraCollectionState, OuraSyncState, OuraSyncStats, OURA_SYNC_SECS};
pub use photos::Photo;
pub use boards::{parse_board, render_board, slugify as board_slugify, Board, BoardSeries, Panel, PanelKind, BOARDS_DIR};
pub use columns::{Agg, ColumnInfo, TableColumns, TableInfo, RECORDS_COLUMN};
pub use query::{list_sources, record_date, SourceInfo, StreamInfo, StreamPage, STREAM_PAGE_MAX};
pub use podcasts::{
    diff_episodes, podcasts_db_mtime, read_podcast_library, read_podcasts_via_copy,
    PodcastEpisode, PodcastEvent, PodcastSnapshotStats,
};
pub use location::Fix;
pub use reading::{Highlight, Item};
pub use registry::{
    Advance, Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectMethodInfo, ConnectStatus,
    ConnectedAccount, ConnectionDef, ConnectionStatusRow, Gate, ImportInfo, ImportOutcome,
    ImportParam, ImportSpec, IntegrationDef, PullOutcome,
};
pub use runner::{
    collector_plist_path, run_sync, CollectorState, CollectorStatus, SyncControl,
    COLLECTOR_LABEL, POLL_SECS,
};
pub use screen_time::{
    decode_infocus, decode_nowplaying, is_idle_bundle, pair_nowplaying, pair_sessions,
    screen_time_mtime, screen_time_permission_ok, DeviceInfo, InFocusEvent, NowPlayingEvent,
    NowPlayingPlay,
    RawNowPlaying, RawSession, ScreenTimeAppUsage, ScreenTimeDeviceUsage, ScreenTimeSession,
    ScreenTimeSummary, ScreenTimeSyncState, ScreenTimeSyncStats, NOWPLAYING_SOURCE,
    SCREEN_TIME_SOURCE,
};
pub use segb::{proto_fields, read_segb, ProtoValue, SegbEntry, SegbSegment, APPLE_EPOCH_OFFSET_S};
pub use slack::SlackImportStats;
pub use social::{Media, Post};
pub use store::{write_atomic, write_atomic_secret, write_json_atomic, JsonlStream, Partition};
pub use sync::google::{GoogleAccountInfo, GoogleStatus};
pub use sync::oauth::{AppCredentials, TokenSet};
pub use sync::{SyncReport, SyncStatus};
pub use tasks::{
    ProjectCount, Subtask, Task, TaskEvent, TasksOverview, TasksSyncState, TasksSyncStats,
    TASKS_SYNC_SECS,
};
pub use time_entries::TimeEntry;
pub use travel::Segment;
pub use vault::{ArtifactMeta, Vault};
pub use voice::Recording;
pub use weather::{
    should_observe, WeatherDay, WeatherLocation, WeatherObservation, WeatherSyncState,
    WeatherSyncStats, WEATHER_OBSERVE_SECS,
};
