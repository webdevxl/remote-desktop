//! macOS privacy (TCC) checks. Grants are tied to the app's code signature, so run the signed
//! `.app` bundle (see scripts/bundle.sh) rather than the bare binary.

use objc2_core_graphics::{CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// Whether this process may capture the screen (needed to be a host).
pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Shows the system prompt (once per app) and returns the current state.
pub fn request_screen_capture() -> bool {
    CGRequestScreenCaptureAccess()
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Whether this process may post mouse and keyboard events, i.e. be controlled remotely. The
/// switch is under Privacy & Security → Accessibility. `AXIsProcessTrusted` notices a grant
/// while the app runs; `CGPreflightPostEventAccess` only after a relaunch, but covers the
/// narrower "post events" grant.
pub fn input_control_allowed() -> bool {
    let trusted = unsafe { AXIsProcessTrusted() };
    trusted || CGPreflightPostEventAccess()
}
