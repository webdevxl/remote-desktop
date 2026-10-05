// C interface to the LanKVM Rust core (crates/core/src/ffi.rs).
//
// Strings returned by lk_* functions are JSON, owned by the caller, and must be released with
// lk_string_free. Events arrive as JSON on the callback, on arbitrary threads.

#ifndef LANKVM_H
#define LANKVM_H

#include <stdbool.h>
#include <stdint.h>

typedef void (*lk_event_callback)(const char *json, void *ctx);

// Starts the core. Returns NULL on success, or an error message.
char *lk_start(lk_event_callback callback, void *ctx);
void lk_string_free(char *s);

char *lk_this_mac(void);
char *lk_host_status(void);
char *lk_paired_devices(void);
char *lk_recent_hosts(void);

// Viewer sessions. max_width/max_height: this screen's size in pixels; max_fps: its refresh rate.
uint64_t lk_connect(const char *target, uint32_t max_width, uint32_t max_height, uint32_t max_fps);
void lk_submit_pin(uint64_t session, const char *pin);
void lk_disconnect(uint64_t session);
char *lk_session_stats(uint64_t session);

// Which of the host's displays a session shows. Both return the request id that the answering
// `display` event carries (0 if there's no such session: nothing will answer). That event also
// says what the session shows now and whether it may ask for a virtual display; `connected`
// already says both for the start.
// The host's own (main) screen:
uint32_t lk_show_main_display(uint64_t session);
// A display the host makes for this Mac, width × height pixels (even). Retina (hidpi) draws it at
// 2x, so its desktop looks like width/2 × height/2. refresh_hz: how often the host draws it.
#define LK_ARRANGE_EXTEND 0 // next to the host's own displays
#define LK_ARRANGE_MAIN 1   // and the main display: menu bar, Dock and new windows go to it
#define LK_ARRANGE_ONLY 2   // the host's own displays mirror it, so every window is on it
uint32_t lk_show_virtual_display(uint64_t session, uint32_t width, uint32_t height, bool hidpi,
                                 uint32_t refresh_hz, uint8_t arrangement);
// Sizes a host makes (it refuses others with LK_DISPLAY_INVALID).
#define LK_DISPLAY_MIN_WIDTH 640
#define LK_DISPLAY_MIN_HEIGHT 480
#define LK_DISPLAY_MAX_SIDE 8192
#define LK_DISPLAY_MAX_PIXELS 35389440 // 8192 × 4320
#define LK_DISPLAY_MAX_ASPECT 4        // width : height, either way
// Why a session doesn't show the display it asked for (the `display` event's `reason`), or can't
// ask for a virtual display (its info's `displayAvailable`). The event's message says it in words.
#define LK_DISPLAY_NONE 0
#define LK_DISPLAY_INVALID 1
#define LK_DISPLAY_NOT_ALLOWED 2     // the host lets paired Macs only view it
#define LK_DISPLAY_UNSUPPORTED 3     // the host's macOS can't make virtual displays
#define LK_DISPLAY_FAILED 4          // trying again may work
#define LK_DISPLAY_REMOVED_BY_HOST 5 // the host's user removed it
#define LK_DISPLAY_GONE 6
#define LK_DISPLAY_SAME_MAC 7        // only an extended display, on the host's own Mac
#define LK_DISPLAY_IN_USE 8          // another device controls the host: no main or only display
#define LK_DISPLAY_NO_VIDEO 9
#define LK_DISPLAY_TOO_MANY 10

// Rendering into a CAMetalLayer, sized in pixels. The layer is retained until detach.
void lk_attach_view(uint64_t session, void *metal_layer, uint32_t width, uint32_t height);
void lk_resize_view(uint64_t session, uint32_t width, uint32_t height);
void lk_detach_view(uint64_t session);

// Remote control, viewer side. Positions are on the remote screen, 0...1 from its left/top edge.
// lk_set_control returns the request id that the answering `control` event carries.
uint32_t lk_set_control(uint64_t session, bool on, bool take_over);
// While controlling: whether the viewer window has the focus (the host shows its cursor in the
// video while it doesn't).
void lk_set_focus(uint64_t session, bool forwarding);
// Whether this Mac shares its clipboard with the Macs it controls, for every session (on until
// told otherwise). A `clipboardTooLarge` event says when one was too big to share.
void lk_set_share_clipboard(bool on);
// Whether to share this Mac's microphone with the session's host, whose apps then hear it as
// "LanKVM Microphone" (off on connecting). `microphone` events say whether the host plays it, or
// why it doesn't or wouldn't (`reason`, and in words `message`); a refusal turns it off.
void lk_set_microphone(uint64_t session, bool on);
#define LK_MIC_NONE 0
#define LK_MIC_NOT_INSTALLED 1    // the host doesn't have the LanKVM Microphone driver
#define LK_MIC_TURNED_OFF 2       // the host lets paired Macs only view it
#define LK_MIC_FAILED 3           // trying again may work
#define LK_MIC_CAPTURE_FAILED 100 // this Mac's microphone couldn't be opened
#define LK_MIC_LOOPBACK 101       // this Mac's microphone is LanKVM Microphone itself
#define LK_MIC_NO_INPUT 102       // this Mac has no microphone
void lk_input_mouse_move(uint64_t session, double x, double y);
void lk_input_mouse_button(uint64_t session, uint8_t button, bool down, uint8_t clicks, double x, double y);

// One scroll event, copied from the NSEvent's CGEvent fields (y is the vertical axis).
typedef struct {
    double x, y;
    int32_t lines_y, lines_x;     // kCGScrollWheelEventDeltaAxis1/2
    double fixed_y, fixed_x;      // ...FixedPtDeltaAxis1/2
    int32_t pixels_y, pixels_x;   // ...PointDeltaAxis1/2
    bool continuous;              // precise (trackpad) deltas
    uint8_t phase;                // CGScrollPhase
    uint8_t momentum;             // CGMomentumScrollPhase
    bool inverted;                // natural scrolling
} lk_scroll;

void lk_input_scroll(uint64_t session, const lk_scroll *scroll);

// Trackpad gestures. Phases are IOHIDEventPhaseBits (the numbers of CGScrollPhase, not of
// NSEvent.Phase); a gesture in progress counts as held for heartbeats.
#define LK_PHASE_BEGAN 1
#define LK_PHASE_CHANGED 2
#define LK_PHASE_ENDED 4
#define LK_PHASE_CANCELLED 8
// Dock swipe axes: the Dock gesture's motion field (123).
#define LK_DOCK_HORIZONTAL 1
#define LK_DOCK_VERTICAL 2
#define LK_DOCK_PINCH 3
// A swipe or pinch the Dock acts on (Spaces, Mission Control...), exactly as this Mac's trackpad
// reported it: progress since it began (field 124), exit velocities (129, 130), inverted (136).
void lk_input_dock_swipe(uint64_t session, uint8_t axis, uint8_t phase, double progress,
                         double velocity_x, double velocity_y, bool inverted);
// App gestures at a position on the remote screen: NSEvent's magnification, rotation (degrees),
// and a swipe event's deltaX/deltaY.
void lk_input_magnify(uint64_t session, double x, double y, uint8_t phase, double delta);
void lk_input_rotate(uint64_t session, double x, double y, uint8_t phase, double degrees);
void lk_input_smart_magnify(uint64_t session, double x, double y);
void lk_input_navigation_swipe(uint64_t session, double x, double y, int8_t dx, int8_t dy);

// Things to do on the remote Mac as a whole (menu items).
#define LK_SYSTEM_MISSION_CONTROL 1
#define LK_SYSTEM_APP_EXPOSE 2
#define LK_SYSTEM_SHOW_DESKTOP 3
#define LK_SYSTEM_LAUNCHPAD 4
#define LK_SYSTEM_PREVIOUS_SPACE 5
#define LK_SYSTEM_NEXT_SPACE 6
void lk_input_system_action(uint64_t session, uint16_t action);
// A non-modifier key by virtual key code; modifiers are sent as state with lk_input_modifiers.
void lk_input_key(uint64_t session, uint16_t code, bool down, bool repeat);
void lk_input_modifiers(uint64_t session, uint64_t flags);
void lk_input_release_all(uint64_t session);
// Every 250 ms from the UI thread while anything is held or a gesture is in progress, so the
// host releases it if we hang.
void lk_input_heartbeat(uint64_t session);
// How many LanKVM hosts the input sent next has passed through (0: made on this Mac); send it
// when it changes.
void lk_input_relayed(uint64_t session, uint8_t depth);

// Before quitting: releases everything held on remote Macs and on this one. Blocks briefly.
void lk_shutdown(void);

// Host side.
void lk_kick_viewer(uint64_t viewer);
void lk_deny_pairing(uint64_t request);
void lk_forget_device(const char *kind, const char *fingerprint);
// Macs this one controls (fingerprint in hex). The name this Mac calls one by ("" or NULL: its
// own), and how "lankvm:<fingerprint>" connects to it: "auto", "local" or "internet".
void lk_set_host_alias(const char *fingerprint, const char *alias);
void lk_set_host_connection(const char *fingerprint, const char *via);
void lk_stop_control(uint64_t viewer);
void lk_stop_all_control(void);
void lk_set_allow_control(bool allow);
// Opt-in: lets paired Macs connect over the internet, and asks the router to forward the port.
void lk_set_internet_access(bool on);
// The address other Macs use over the internet (dynamic DNS name or IP, optional :port); "" clears.
void lk_set_public_address(const char *address);
// The LanKVM server ("host:port") that introduces paired Macs over the internet; "" turns the LanKVM server off.
void lk_set_rendezvous_server(const char *address);
// Whether this Mac meets paired Macs through the BitTorrent DHT too, with no server (as host and as viewer).
void lk_set_dht(bool on);
// Whether macOS lets LanKVM post input (Privacy & Security → Accessibility).
bool lk_control_permission(void);
// After installing or removing the LanKVM Microphone driver here: viewers learn whether they can
// share their microphones with this Mac (the host status's microphoneReady says it too).
void lk_microphone_driver_changed(void);
// Removes a virtual display made for a viewer (0: all of them); its viewers go back to this Mac's
// own screen. Returns at once; a `hostChanged` event follows.
void lk_remove_virtual_display(uint32_t display_id);
// Whether this user's session has the screen (fast user switching). While it doesn't, viewers lose
// their virtual displays (they hold this user's windows) and can't add one.
void lk_set_console_active(bool active);

// Screen Recording permission.
bool lk_screen_capture_allowed(void);
bool lk_verify_screen_capture(void);
bool lk_request_screen_capture(void);

#endif
