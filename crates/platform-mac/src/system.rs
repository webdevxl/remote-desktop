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
