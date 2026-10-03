//! Creates a real virtual display for a moment: next to this Mac's displays, never main or mirrored
//! (a test process has no AppKit event loop, and arranging displays there would keep the display
//! until the process exits). It shows up in Displays settings for about a second.
//!
//! cargo test -p platform-mac --test virtual_display -- --ignored --nocapture

use std::time::{Duration, Instant};

use platform_mac::inject::Bounds;
use platform_mac::virtual_display::{Mode, VirtualDisplay, is_active, is_lankvm, online_displays, pixel_size};

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "creates a display"]
fn a_virtual_display_comes_and_goes() {
    let retina = Mode { width: 6144, height: 2560, hidpi: true, refresh_hz: 60 };
    let mut display = VirtualDisplay::create("LanKVM test", 0x7e57, retina).expect("create");
    let id = display.id();
    wait_until("it is online", || online_displays().contains(&id));
    assert!(is_lankvm(id) && is_active(id));
    assert_eq!(pixel_size(id), Some((6144, 2560)));
    let bounds = Bounds::of_display(id);
    assert_eq!((bounds.width, bounds.height), (3072.0, 1280.0), "a Retina mode is laid out in points");

    // A new mode applies in place: same display, new size.
    let smaller = Mode { width: 2560, height: 1440, hidpi: false, refresh_hz: 120 };
    display.set_mode(smaller).expect("set mode");
    assert_eq!(display.id(), id);
    assert_eq!(pixel_size(id), Some((2560, 1440)));
    assert_eq!(Bounds::of_display(id).width, 2560.0);

    drop(display);
    wait_until("it is gone", || !online_displays().contains(&id));
    assert!(!Bounds::of_display(id).is_usable());
}
