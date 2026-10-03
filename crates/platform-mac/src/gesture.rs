//! Trackpad gestures: reading the direction conventions of this Mac's Dock gestures, and posting
//! gestures received from a viewer.
//!
//! PLACEHOLDER (contract only): the real implementation replaces this file.

use protocol::DockAxis;

/// The factor between this Mac's own Dock gesture values (progress and velocities, as its
/// trackpad reports them and as its Dock expects them posted) and the wire's. The same factor
/// is used both ways, so two Macs set up alike replay each other's gestures exactly.
pub fn dock_direction(_axis: DockAxis) -> f64 {
    1.0
}
