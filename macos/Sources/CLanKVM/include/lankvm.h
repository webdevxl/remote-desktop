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

// Viewer sessions. max_width/max_height: this screen's size in pixels.
uint64_t lk_connect(const char *target, uint32_t max_width, uint32_t max_height);
void lk_submit_pin(uint64_t session, const char *pin);
void lk_disconnect(uint64_t session);
char *lk_session_stats(uint64_t session);

// Rendering into a CAMetalLayer, sized in pixels. The layer is retained until detach.
void lk_attach_view(uint64_t session, void *metal_layer, uint32_t width, uint32_t height);
void lk_resize_view(uint64_t session, uint32_t width, uint32_t height);
void lk_detach_view(uint64_t session);

// Host side.
void lk_kick_viewer(uint64_t viewer);
void lk_deny_pairing(uint64_t request);
void lk_forget_device(const char *kind, const char *fingerprint);

// Screen Recording permission.
bool lk_screen_capture_allowed(void);
bool lk_verify_screen_capture(void);
bool lk_request_screen_capture(void);

#endif
