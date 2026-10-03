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
// A non-modifier key by virtual key code; modifiers are sent as state with lk_input_modifiers.
void lk_input_key(uint64_t session, uint16_t code, bool down, bool repeat);
void lk_input_modifiers(uint64_t session, uint64_t flags);
void lk_input_release_all(uint64_t session);
// Every 250 ms from the UI thread while anything is held, so the host releases it if we hang.
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
void lk_stop_control(uint64_t viewer);
void lk_stop_all_control(void);
void lk_set_allow_control(bool allow);
// Whether macOS lets LanKVM post input (Privacy & Security → Accessibility).
bool lk_control_permission(void);

// Screen Recording permission.
bool lk_screen_capture_allowed(void);
bool lk_verify_screen_capture(void);
bool lk_request_screen_capture(void);

#endif
