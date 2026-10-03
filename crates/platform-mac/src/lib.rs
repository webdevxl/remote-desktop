//! macOS implementations: screen capture, hardware video encode/decode, GPU import,
//! permissions and clocks. Everything platform-specific lives here so other platforms can
//! slot in beside it later.

pub mod capture;
pub mod clock;
pub mod cursor;
pub mod decoder;
pub mod encoder;
pub mod gesture;
pub mod gpu;
pub mod inject;
pub mod keys;
mod nal;
pub mod system;
pub mod permissions;
mod util;
pub mod virtual_display;

pub use objc2_core_foundation::CFRetained;
pub use objc2_core_video::CVPixelBuffer;
