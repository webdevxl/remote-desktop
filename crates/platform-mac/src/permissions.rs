//! macOS privacy (TCC) checks. Grants are tied to the app's code signature, so run the signed
//! `.app` bundle (see scripts/bundle.sh) rather than the bare binary.

use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};

/// Whether this process may capture the screen (needed to be a host).
pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Shows the system prompt (once per app) and returns the current state.
pub fn request_screen_capture() -> bool {
    CGRequestScreenCaptureAccess()
}
