//! CoreLocation bridge — the OS half of the weather collector's
//! "current location" question.
//!
//! Same shape as [`crate::eventkit`]: a tiny TCC-gated bridge that turns
//! framework objects into plain data, with everything testable kept out of
//! it. Location is a per-service TCC prompt keyed to the responsible
//! process; the prompt only renders when that process carries
//! `NSLocationWhenInUseUsageDescription` (Tauri app: Info.plist; a headless binary: the
//! embedded `__TEXT,__info_plist` section). Like Calendars/Reminders, the
//! System Settings → Location Services pane lists an app only after it has
//! requested once.
//!
//! Run-loop contract: CLLocationManager delivers delegate callbacks on the
//! run loop of the thread that created it — so [`current_location`] creates
//! the manager on the *calling* thread and pumps that thread's run loop
//! while it waits (unlike EventKit, whose completions arrive on its own
//! queue). Callers must not hold the watcher tick hostage: the waits here
//! are bounded and a result of `None` is an expected, retryable outcome.

pub use crate::eventkit::AuthStatus;

/// A resolved location: coordinates plus how old the fix is. Weather is
/// city-scale, so even a fairly stale fix is useful — callers decide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocationFix {
    pub lat: f64,
    pub lon: f64,
    /// Seconds since the OS recorded this fix (0 for a live update).
    pub age_secs: i64,
}

pub fn auth_status() -> AuthStatus {
    imp::auth_status()
}

/// Ask for location access (fires the TCC prompt when not-determined and
/// the process carries the usage string). Waits up to `wait_secs` for the
/// answer — an already-decided request resolves on the first poll; a live
/// prompt usually outlives the wait, in which case the caller skips this
/// pass and picks the grant up on the next one.
pub fn request_access(wait_secs: u64) -> bool {
    imp::request_access(wait_secs)
}

/// The current location, waiting up to `wait_secs` for a live fix. A cached
/// fix newer than 15 minutes is returned immediately; a stale cached fix is
/// the fallback when no live update arrives in time. `None` when access is
/// not granted or no fix exists at all.
pub fn current_location(wait_secs: u64) -> Option<LocationFix> {
    imp::current_location(wait_secs)
}

/// How fresh a cached fix can be and still skip the live update entirely.
const FRESH_FIX_SECS: i64 = 15 * 60;

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::mpsc::{self, Sender};
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
    use objc2_core_location::{
        CLAuthorizationStatus, CLLocation, CLLocationManager, CLLocationManagerDelegate,
    };
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};

    use super::{AuthStatus, LocationFix, FRESH_FIX_SECS};

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopRunInMode(
            mode: core_foundation::string::CFStringRef,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
        static kCFRunLoopDefaultMode: core_foundation::string::CFStringRef;
    }

    /// Run the calling thread's CFRunLoop for at most `seconds` (one slice).
    /// CoreLocation delivers its callbacks on the *calling* thread's run
    /// loop, so the waits below pump with this between checks. Each slice
    /// drains an autorelease pool: servicing the run loop autoreleases
    /// CF/NSObject temporaries that a non-AppKit thread never drains otherwise.
    fn run_loop_slice(seconds: f64) {
        objc2::rc::autoreleasepool(|_| unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0);
        });
    }

    pub fn auth_status() -> AuthStatus {
        // The class getter is deprecated in favor of the instance property,
        // but the instance property requires a manager — which is only
        // meaningful on a run-loop thread. The class getter answers the same
        // question from any thread.
        #[allow(deprecated)]
        let status = unsafe { CLLocationManager::authorizationStatus_class() };
        match status {
            CLAuthorizationStatus::NotDetermined => AuthStatus::NotDetermined,
            // macOS reports kCLAuthorizationStatusAuthorized(Always); treat
            // WhenInUse the same for forward compatibility.
            CLAuthorizationStatus::AuthorizedAlways
            | CLAuthorizationStatus::AuthorizedWhenInUse => AuthStatus::Granted,
            _ => AuthStatus::Denied,
        }
    }

    pub fn request_access(wait_secs: u64) -> bool {
        if auth_status() == AuthStatus::Granted {
            return true;
        }
        let manager = unsafe { CLLocationManager::new() };
        unsafe { manager.requestWhenInUseAuthorization() };
        // There is no completion block — poll the status while pumping this
        // thread's run loop so the manager can process the answer.
        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        loop {
            run_loop_slice(0.25);
            match auth_status() {
                AuthStatus::Granted => return true,
                AuthStatus::Denied => return false,
                AuthStatus::NotDetermined if Instant::now() >= deadline => return false,
                AuthStatus::NotDetermined => {}
            }
        }
    }

    struct Ivars {
        tx: Sender<(f64, f64)>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "TroveLocationDelegate"]
        #[ivars = Ivars]
        struct LocationDelegate;

        unsafe impl NSObjectProtocol for LocationDelegate {}

        unsafe impl CLLocationManagerDelegate for LocationDelegate {
            #[unsafe(method(locationManager:didUpdateLocations:))]
            unsafe fn did_update_locations(
                &self,
                _manager: &CLLocationManager,
                locations: &NSArray<CLLocation>,
            ) {
                if let Some(loc) = locations.lastObject() {
                    let c = unsafe { loc.coordinate() };
                    let _ = self.ivars().tx.send((c.latitude, c.longitude));
                }
            }

            // Without this handler an error (e.g. Wi-Fi off, locationd
            // unavailable) would raise an unrecognized-selector panic at
            // delivery; swallowing it lets the wait time out into the
            // cached-fix fallback instead.
            #[unsafe(method(locationManager:didFailWithError:))]
            unsafe fn did_fail(&self, _manager: &CLLocationManager, _error: &NSError) {}
        }
    );

    impl LocationDelegate {
        fn new(tx: Sender<(f64, f64)>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { tx });
            unsafe { msg_send![super(this), init] }
        }
    }

    fn fix_from(loc: &CLLocation) -> LocationFix {
        let c = unsafe { loc.coordinate() };
        let recorded = unsafe { loc.timestamp().timeIntervalSince1970() };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(recorded);
        LocationFix {
            lat: c.latitude,
            lon: c.longitude,
            age_secs: (now - recorded).max(0.0) as i64,
        }
    }

    pub fn current_location(wait_secs: u64) -> Option<LocationFix> {
        if auth_status() != AuthStatus::Granted {
            return None;
        }
        let manager = unsafe { CLLocationManager::new() };
        // locationd caches the last fix process-independently; on a mostly
        // stationary Mac this answers instantly without powering anything up.
        let cached = unsafe { manager.location() }.map(|l| fix_from(&l));
        if let Some(f) = cached {
            if f.age_secs <= FRESH_FIX_SECS {
                return Some(f);
            }
        }
        let (tx, rx) = mpsc::channel();
        let delegate = LocationDelegate::new(tx);
        unsafe { manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        unsafe { manager.startUpdatingLocation() };
        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        let mut live = None;
        while Instant::now() < deadline {
            run_loop_slice(0.25);
            if let Ok((lat, lon)) = rx.try_recv() {
                live = Some(LocationFix { lat, lon, age_secs: 0 });
                break;
            }
        }
        unsafe {
            manager.stopUpdatingLocation();
            manager.setDelegate(None);
        }
        // A stale cached fix still beats nothing: weather is city-scale and
        // Macs rarely move between fixes.
        live.or(cached)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{AuthStatus, LocationFix};

    pub fn auth_status() -> AuthStatus {
        AuthStatus::Denied
    }

    pub fn request_access(_wait_secs: u64) -> bool {
        false
    }

    pub fn current_location(_wait_secs: u64) -> Option<LocationFix> {
        None
    }
}
