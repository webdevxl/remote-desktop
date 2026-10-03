//! Process-level macOS settings.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
#[allow(deprecated)] // NSHost is the only Foundation API that returns the user-facing computer name.
use objc2_foundation::NSHost;
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};

/// The computer name shown in Sharing settings, e.g. "Alex's MacBook Pro".
#[allow(deprecated)]
pub fn device_name() -> String {
    NSHost::currentHost()
        .localizedName()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "Mac".to_string())
}

/// Keeps App Nap and timer coalescing from throttling the process while it's in the background
/// (a host is usually not the frontmost app). Hold the returned token for the process lifetime.
pub struct ActivityToken(#[allow(dead_code)] Retained<ProtocolObject<dyn NSObjectProtocol>>);

pub fn begin_latency_critical_activity() -> ActivityToken {
    let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep | NSActivityOptions::LatencyCritical;
    let token = NSProcessInfo::processInfo()
        .beginActivityWithOptions_reason(options, &NSString::from_str("Remote desktop host"));
    ActivityToken(token)
}

/// Runs the calling thread at the highest QoS, like the UI thread, so the scheduler wakes it
/// promptly and keeps it on a performance core. For threads on the input-to-photon path.
pub fn set_thread_interactive() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

/// Tells macOS a (remote) user is active: wakes the display and postpones display sleep and the
/// screen saver, as local input would. Cheap; call at most about once a second.
pub fn declare_user_activity() {
    type IOPMAssertionID = u32;
    const K_IOPM_USER_ACTIVE_REMOTE: u32 = 1;
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: &objc2_core_foundation::CFString,
            user_type: u32,
            id: *mut IOPMAssertionID,
        ) -> i32;
    }
    use std::sync::Mutex;
    static ID: Mutex<IOPMAssertionID> = Mutex::new(0);
    let name = objc2_core_foundation::CFString::from_static_str("LanKVM remote control");
    let mut id = ID.lock().unwrap();
    unsafe { IOPMAssertionDeclareUserActivity(&name, K_IOPM_USER_ACTIVE_REMOTE, &mut *id) };
}

/// Whether this is the process's main thread.
pub fn is_main_thread() -> bool {
    unsafe extern "C" {
        fn pthread_main_np() -> i32;
    }
    unsafe { pthread_main_np() != 0 }
}

/// Keeps this Mac's displays from sleeping after its idle time while it lives. A virtual display
/// stops being drawn (or goes away) when the displays sleep, and a viewer only watching it sends
/// no input that would count as activity.
pub struct DisplaysAwake(u32);

impl DisplaysAwake {
    pub fn new(reason: &str) -> Option<Self> {
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPMAssertionCreateWithName(
                kind: &objc2_core_foundation::CFString,
                level: u32,
                name: &objc2_core_foundation::CFString,
                id: *mut u32,
            ) -> i32;
        }
        const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
        let kind = objc2_core_foundation::CFString::from_static_str("PreventUserIdleDisplaySleep");
        let name = objc2_core_foundation::CFString::from_str(reason);
        let mut id = 0;
        let status = unsafe { IOPMAssertionCreateWithName(&kind, K_IOPM_ASSERTION_LEVEL_ON, &name, &mut id) };
        if status != 0 {
            tracing::warn!(status, "can't keep the displays awake");
            return None;
        }
        Some(Self(id))
    }
}

impl Drop for DisplaysAwake {
    fn drop(&mut self) {
        #[link(name = "IOKit", kind = "framework")]
        unsafe extern "C" {
            fn IOPMAssertionRelease(id: u32) -> i32;
        }
        unsafe { IOPMAssertionRelease(self.0) };
    }
}
