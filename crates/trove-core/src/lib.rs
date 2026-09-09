//! trove-core: the vault.
//!
//! A Trove vault is a plain folder of open-format files (markdown, CSV,
//! JSONL). This crate owns the vault layout and all reads/writes to it, so
//! the GUI app and the background collector daemon share one implementation
//! and the files stay the single source of truth.

pub mod activity;
pub mod actual_budget;
pub mod ads;
pub mod airbnb;
pub mod airnow;
pub mod airthings;
pub mod alfred;
pub mod amazfit;
pub mod amazing_marvin;
pub mod amazon;
pub mod amazon_alexa;
pub mod ambient_weather;
pub mod ancestrydna;
pub mod apple_app_store;
pub mod apple_card;
pub mod apple_contacts;
pub mod apple_homekit;
pub mod apple_journal;
pub mod apple_mail;
pub mod apple_maps;
pub mod apple_notes;
pub mod apple_photos;
pub mod apple_significant_locations;
pub mod apple_voice_memos;
pub mod apple_voicemail;
pub mod aranet;
pub mod arc_timeline;
pub mod asana;
pub mod audible;
pub mod august_yale;
pub mod awair;
pub mod awardwallet;
pub mod bandcamp;
pub mod bear;
pub mod bearable;
pub mod beeper;
pub mod bereal;
pub mod bitbucket;
pub mod bitcoin;
pub mod blitzortung;
pub mod bluesky;
pub mod boardgamegeek;
pub mod books;
pub mod box_;
pub mod browser;
pub mod browser_ext;
pub mod browser_searches;
pub mod bumble;
pub mod cal_com;
pub mod caldav;
pub mod calendar;
pub mod calendly;
pub mod calls;
pub mod capacities;
pub mod cash_app;
pub mod chatgpt;
pub mod chess_com;
pub mod claude_code;
pub mod clay;
pub mod clip_embeddings;
pub mod cloud_folder;
pub mod clockify;
pub mod cms_blue_button;
pub mod coinbase;
pub mod connect;
pub mod contacts;
pub mod contracts;
pub mod corelocation;
pub mod coros;
pub mod correspondence;
pub mod craft;
pub mod cronometer;
pub mod crypto_tax_exports;
pub mod cursor;
pub mod day_one;
pub mod deezer;
pub mod dental_records;
pub mod dex;
pub mod dexcom;
pub mod discord;
pub mod disney_hulu_max;
pub mod docusign;
pub mod drafts;
pub mod dropbox;
pub mod ecobee;
pub mod eight_sleep;
pub mod email;
pub mod emporia;
pub mod enphase;
pub mod environment;
pub mod epic_games;
pub mod epic_mychart;
pub mod ethereum;
pub mod eventkit;
pub mod evernote;
pub mod exif_import;
pub mod facebook;
pub mod facebook_messenger;
pub mod fastmail;
pub mod fathom;
pub mod feedly;
pub mod fidelity;
pub mod finance;
pub mod fireflies;
pub mod fitbit;
pub mod flickr;
pub mod flight_emails;
pub mod flighty;
pub mod freestyle_libre;
pub mod garmin;
pub mod genetics_variants;
pub mod github;
pub mod github_copilot;
pub mod gitlab;
pub mod gmail;
pub mod gocardless;
pub mod gog_galaxy;
pub mod goodreads;
pub mod google_analytics;
pub mod google_books;
pub mod google_calendar;
pub mod google_chat;
pub mod google_contacts;
pub mod google_drive;
pub mod google_keep;
pub mod google_maps;
pub mod google_meet;
pub mod google_nest;
pub mod google_photos;
pub mod google_pollen;
pub mod google_takeout;
pub mod google_tasks;
pub mod google_timeline;
pub mod google_voice;
pub mod granola;
pub mod green_button;
pub mod grocery_loyalty;
pub mod groupme;
pub mod habitica;
pub mod habitify;
pub mod habits;
pub mod hacker_news;
pub mod hardcover;
pub mod harvest;
pub mod health;
pub mod health_medical;
pub mod health_nutrition;
pub mod health_unified;
pub mod hinge;
pub mod home;
pub mod home_assistant;
pub mod honeywell_resideo;
pub mod hypothesis;
pub mod icloud_contacts;
pub mod icloud_drive;
pub mod imap;
pub mod imdb;
pub mod imessage;
pub mod inoreader;
pub mod instagram;
pub mod instapaper;
pub mod integrations;
pub mod interactive_brokers;
pub mod irc;
pub mod iterm2;
pub mod jellyfin;
pub mod jetbrains;
pub mod jira;
pub mod kagi;
pub mod kindle;
pub mod kraken;
pub mod krisp;
pub mod lab_pdf_import;
pub mod labcorp;
pub mod lastfm;
pub mod letterboxd;
pub mod levels_health;
pub mod libby;
pub mod lichess;
pub mod life360;
pub mod lifesum;
pub mod line;
pub mod linear;
pub mod linkding;
pub mod linkedin;
pub mod listenbrainz;
pub mod literal;
pub mod local_git;
pub mod location;
pub mod logseq;
pub mod lunch_money;
pub mod lutron_caseta;
pub mod macos_barometer;
pub mod macos_downloads;
pub mod macos_fsevents;
pub mod macos_microphone;
pub mod macos_recent_files;
pub mod macos_screenshots;
pub mod macrofactor;
pub mod mastodon;
pub mod matrix;
pub mod matter;
pub mod media;
pub mod mediaremote;
pub mod medisafe;
pub mod meetings;
pub mod meta_encoding;
pub mod microsoft_teams;
pub mod microsoft_todo;
pub mod moen_flo;
pub mod monarch_money;
pub mod monica;
pub mod motion;
pub mod music;
pub mod music_library;
pub mod music_listener;
pub mod myfitnesspal;
pub mod myflightradar24;
pub mod nasa_firms;
pub mod navidrome;
pub mod netatmo;
pub mod netflix;
pub mod netnewswire;
pub mod nightscout;
pub mod nintendo_switch;
pub mod noaa_cdo;
pub mod noaa_co_ops;
pub mod noaa_ndbc;
pub mod noaa_swpc;
pub mod noom;
pub mod normalizer;
pub mod notes;
pub mod notion;
pub mod notion_airtable;
pub mod nws;
pub mod obsidian;
pub mod okcupid;
pub mod omnifocus;
pub mod omnivore;
pub mod omron;
pub mod onedrive;
pub mod onenote;
pub mod org_mode;
pub mod otter;
pub mod oura;
pub mod outlook;
pub mod outlook_calendar;
pub mod overcast;
pub mod overland;
pub mod owntracks;
pub mod pandora;
pub mod password_manager;
pub mod paypal;
pub mod pharmacy_prescriptions;
pub mod philips_hue;
pub mod photos;
pub mod pinboard;
pub mod pinterest;
pub mod plaid;
pub mod plausible;
pub mod playstation;
pub mod plex;
pub mod pocket;
pub mod pocket_casts;
pub mod podcasts;
pub mod polar;
pub mod prime_video;
pub mod protonmail;
pub mod purpleair;
pub mod px500;
pub mod qbserve;
pub mod quest_diagnostics;
pub mod raindrop;
pub mod read_ai;
pub mod reading;
pub mod readwise;
pub mod reddit;
pub mod reeder;
pub mod reflect;
pub mod registry;
pub mod renpho;
pub mod retroachievements;
pub mod ring;
pub mod roam;
pub mod robinhood;
pub mod roborock;
pub mod runner;
pub mod sampler;
pub mod samsung_health;
pub mod schwab;
pub mod screen_time;
pub mod segb;
pub mod sense_energy;
pub mod shazam;
pub mod shell_history;
pub mod signal;
pub mod simkl;
pub mod simplenote;
pub mod skype;
pub mod slack;
pub mod smart_on_fhir;
pub mod smartcar;
pub mod smugmug;
pub mod snapchat;
pub mod snaptrade;
pub mod snipd;
pub mod social;
pub mod soundcloud;
pub mod spotify;
pub mod standard_notes;
pub mod steam;
pub mod stoic;
pub mod store;
pub mod storygraph;
pub mod strava;
pub mod streaks;
pub mod stripe;
pub mod substack;
pub mod sunrise_sunset;
pub mod sunsama;
pub mod suunto;
pub mod swarm;
pub mod switchbot;
pub mod sync;
pub mod tasks;
pub mod telegram;
pub mod teller;
pub mod tesla;
pub mod tesla_energy;
pub mod things;
pub mod threads;
pub mod tidal;
pub mod tiktok;
pub mod time_entries;
pub mod timery;
pub mod timing;
pub mod tinder;
pub mod tldv;
pub mod todoist;
pub mod toggl_track;
pub mod tplink_kasa;
pub mod trakt;
pub mod transit_cards;
pub mod travel;
pub mod trello;
pub mod tripit;
pub mod tumblr;
pub mod tv_time;
#[path = "23andme.rs"]
pub mod twenty_three_and_me;
pub mod twitch;
pub mod ultrahuman;
pub mod ulysses;
pub mod usgs_earthquakes;
pub mod usgs_water;
pub mod usno;
pub mod vanguard;
pub mod vault;
pub mod vcard;
pub mod venmo;
pub mod voice;
pub mod vscode;
pub mod wahoo;
pub mod wakatime;
pub mod wallabag;
pub mod waqi;
pub mod way_of_life;
pub mod weather;
pub mod weatherflow_tempest;
pub mod webex;
pub mod wechat;
pub mod whatsapp;
pub mod whoop;
pub mod wikipedia;
pub mod windsurf;
pub mod withings;
pub mod x_twitter;
pub mod xbox;
pub mod ynab;
pub mod youtube;
pub mod youtube_music;
pub mod zed;
pub mod zoom;

pub use activity::{
    ActivityEvent, ActivitySummary, AppUsage, WatchConfig, Watcher, POLL_SECS,
};
pub use ads::{AdEvent, AdRecord, AdUsage, AdsDaily, AdsSummary};
pub use books::{
    diff_annotations, diff_assets, read_books_via_copy, BookAnnotation, BookAsset, BookEvent,
    BooksSnapshotStats,
};
pub use browser::{
    safari_permission_ok, BrowserSummary, BrowserSyncState, BrowserSyncStats, BrowserVisit,
    DomainUsage, BROWSER_SYNC_SECS,
};
pub use browser_ext::{
    native_host_manifest_path, ExtConfig, ExtSnapshot, LiveSpan, LiveState, TabInfo, TabTracker,
    EXTENSION_ID, EXT_SNAPSHOT_SECS, NATIVE_HOST_NAME,
};
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
pub use music::{
    ArtistUsage, MusicSummary, Play, PlayerEvent, PlayerState, ScrobbleConfig, Scrobbler, TrackInfo,
};
pub use music_library::{
    diff_snapshots, read_library, should_snapshot, FieldChange, LibraryEvent,
    LibrarySnapshotStats, LibraryTrack,
};
pub use music_listener::{pump_main_run_loop, MusicListener};
pub use oura::{OuraCollectionState, OuraSyncState, OuraSyncStats, OURA_SYNC_SECS};
pub use photos::Photo;
pub use podcasts::{
    diff_episodes, podcasts_db_mtime, read_podcast_library, read_podcasts_via_copy,
    PodcastEpisode, PodcastEvent, PodcastSnapshotStats,
};
pub use location::Fix;
pub use reading::{Highlight, Item};
pub use registry::{
    Advance, Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectMethodInfo, ConnectStatus,
    ConnectedAccount, ConnectionDef, ConnectionStatusRow, Gate, ImportInfo, ImportOutcome,
    ImportParam, ImportSpec, IntegrationDef, LiveCollector, PullOutcome,
};
pub use runner::{
    daemon_plist_path, run_watcher, WatchControl, WatcherRole, WatcherState, DAEMON_LABEL,
};
pub use sampler::{request_screen_recording, sample, screen_recording_ok, Sample};
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
