//! Platform listener for Music.app playerInfo notifications.
//!
//! Music.app broadcasts `com.apple.Music.playerInfo` on the *distributed*
//! notification center on every play/pause/stop/track change — a public
//! CoreFoundation mechanism needing no permissions (unlike the private
//! `MediaRemote` API, restricted since macOS 15.4). Verified live 2026-06-10:
//! it also posts `com.apple.iTunes.playerInfo` with an identical payload, so
//! we observe exactly ONE name to avoid double events.
//!
//! ## The main-run-loop contract
//!
//! Empirically (probed on macOS 26): distributed notifications are delivered
//! on the process's **main** run loop, regardless of which thread registered
//! the observer, and observing with a NULL name delivers nothing. So there is
//! no listener thread — [`MusicListener::start`] registers a process-wide
//! observer (once) whose callback forwards decoded events, stamped at
//! delivery time, into a channel. The host must keep the main run loop
//! running:
//!
//! - the GUI app already does (Tauri/AppKit event loop);
//! - headless hosts (troved) call [`pump_main_run_loop`] on the main thread.
//!
//! The observer is registered once and never removed; start/stop just swaps
//! the active channel sender (a global slot), which sidesteps any
//! remove-vs-inflight-callback race entirely. Events arriving with no active
//! listener are dropped — only the single vault lock owner should listen.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use chrono::{DateTime, Local};

use crate::music::PlayerEvent;

/// A decoded notification, stamped with its delivery time.
pub type TimedPlayerEvent = (DateTime<Local>, PlayerEvent);

/// Where the notification callback forwards events while a listener is live.
static ACTIVE: Mutex<Option<Sender<TimedPlayerEvent>>> = Mutex::new(None);

/// Guard for an active listening session. Dropping (or [`stop`]) detaches the
/// channel; the underlying observer registration is process-wide and persists.
///
/// [`stop`]: MusicListener::stop
pub struct MusicListener(());

impl MusicListener {
    /// Begin listening. Returns the receiver playerInfo events arrive on —
    /// provided the process's main run loop is running (see module docs).
    /// A second concurrent listener in one process replaces the first.
    pub fn start() -> (MusicListener, Receiver<TimedPlayerEvent>) {
        let (tx, rx) = channel();
        *ACTIVE.lock().unwrap() = Some(tx);
        imp::ensure_registered();
        (MusicListener(()), rx)
    }

    pub fn stop(self) {}
}

impl Drop for MusicListener {
    fn drop(&mut self) {
        *ACTIVE.lock().unwrap() = None;
    }
}

/// Run the calling thread's CFRunLoop until `stopped()` returns true,
/// checking twice a second. Headless hosts call this on their MAIN thread so
/// distributed notifications get delivered; GUI hosts never need it.
pub fn pump_main_run_loop(stopped: impl Fn() -> bool) {
    while !stopped() {
        imp::run_loop_slice(0.5);
    }
}

/// Run the calling thread's CFRunLoop for at most `seconds` (one slice).
/// For bridges whose callbacks are delivered on the *calling* thread's run
/// loop (CoreLocation) — they pump with this between checks.
pub(crate) fn run_loop_slice(seconds: f64) {
    imp::run_loop_slice(seconds);
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ACTIVE;
    use crate::music::{PlayerState, TrackInfo};
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::Once;

    /// Music also posts `com.apple.iTunes.playerInfo` with an identical
    /// payload — observing both would double every event.
    const NOTIFICATION: &str = "com.apple.Music.playerInfo";

    type CFNotificationCenterRef = *mut c_void;
    type NotificationCallback = extern "C" fn(
        CFNotificationCenterRef,
        *mut c_void,
        CFStringRef,
        *const c_void,
        CFDictionaryRef,
    );

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
        fn CFNotificationCenterAddObserver(
            center: CFNotificationCenterRef,
            observer: *const c_void,
            call_back: NotificationCallback,
            name: CFStringRef,
            object: *const c_void,
            suspension_behavior: isize,
        );
        fn CFRunLoopRunInMode(
            mode: CFStringRef,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
        static kCFRunLoopDefaultMode: CFStringRef;
    }

    const DELIVER_IMMEDIATELY: isize = 4; // kCFNotificationSuspensionBehaviorDeliverImmediately

    static REGISTER: Once = Once::new();

    pub fn ensure_registered() {
        REGISTER.call_once(|| unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            let name = CFString::new(NOTIFICATION);
            CFNotificationCenterAddObserver(
                center,
                std::ptr::null(),
                on_notification,
                name.as_concrete_TypeRef(),
                std::ptr::null(),
                DELIVER_IMMEDIATELY,
            );
        });
    }

    pub fn run_loop_slice(seconds: f64) {
        // Drain an autorelease pool around every slice. Servicing the run loop
        // (distributed-notification delivery, CF mach-port bookkeeping)
        // autoreleases CFString/NSObject temporaries; a GUI host's AppKit loop
        // drains its pool each cycle, but a headless host pumping CF directly
        // has no such pool — without this they accumulate in the thread's
        // top-level pool for the life of the process (observed: a 3-day daemon
        // leaked ~49M allocations / ~2.9 GB of autoreleased CFStrings).
        objc2::rc::autoreleasepool(|_| unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0);
        });
    }

    /// Runs on the main run loop's thread at delivery time.
    extern "C" fn on_notification(
        _center: CFNotificationCenterRef,
        _observer: *mut c_void,
        _name: CFStringRef,
        _object: *const c_void,
        user_info: CFDictionaryRef,
    ) {
        if let Some(tx) = ACTIVE.lock().unwrap().as_ref() {
            let _ = tx.send((chrono::Local::now(), decode(user_info)));
        }
    }

    fn decode(user_info: CFDictionaryRef) -> super::PlayerEvent {
        use super::PlayerEvent;
        if user_info.is_null() {
            return PlayerEvent {
                state: PlayerState::Stopped,
                track: None,
            };
        }
        let dict =
            unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(user_info) };
        let state = match dict_string(&dict, "Player State").as_str() {
            "Playing" => PlayerState::Playing,
            "Paused" => PlayerState::Paused,
            _ => PlayerState::Stopped,
        };
        let name = dict_string(&dict, "Name");
        let artist = dict_string(&dict, "Artist");
        // A bare Stopped (track change boundary) carries only Player State.
        let track = (!name.is_empty() || !artist.is_empty()).then(|| TrackInfo {
            name,
            artist,
            album: dict_string(&dict, "Album"),
            genre: dict_string(&dict, "Genre"),
            duration_secs: dict_i64(&dict, "Total Time").map(|ms| ms as f64 / 1000.0),
            // Delivered as a signed 64-bit number; Music's canonical form is
            // 16 uppercase hex digits. Streaming tracks may omit it — the
            // scrobbler falls back to metadata identity.
            persistent_id: dict_i64(&dict, "PersistentID")
                .map(|id| format!("{:016X}", id as u64))
                .unwrap_or_default(),
        });
        PlayerEvent { state, track }
    }

    fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> String {
        match dict.find(&CFString::new(key)) {
            Some(v) => v
                .downcast::<CFString>()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
        dict.find(&CFString::new(key))?.downcast::<CFNumber>()?.to_i64()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// No Music.app off macOS: nothing to register, the channel just stays empty.
    pub fn ensure_registered() {}

    pub fn run_loop_slice(seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}
