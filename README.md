# LanKVM

Low-latency remote desktop for Macs on the same local network, built as a software network KVM.
Every Mac runs the same app. Others on the LAN can view it, and it can connect to them by IP.

**Status:** first prototype. You enter an IP, enter a PIN once, and see the other Mac's screen.
Keyboard and mouse control is the next milestone.

## How it stays fast

- **Capture → encode on the GPU, no CPU copies.** ScreenCaptureKit delivers IOSurface-backed NV12
  frames straight to the hardware HEVC encoder (VideoToolbox low-latency rate control, no
  B-frames, keyframes only on request).
- **UDP, never TCP, for video.** One QUIC connection (quinn) carries TLS 1.3, a reliable control
  stream, and unreliable datagrams for video. A fixed-window congestion controller is used
  because the LAN doesn't need Cubic's backoff. Lost frames are dropped, never retransmitted,
  and the viewer requests a fresh keyframe.
- **Decode → display with zero copies.** The hardware decoder outputs IOSurfaces. Each plane is
  wrapped as a Metal texture and sampled by a wgpu shader, then presented immediately with no
  frame queue.
- **Measured, not guessed.** The viewer's latency overlay shows capture→screen latency split into
  encode, network, decode and display, using a clock synced to the host.

## Install

```bash
./install.sh
```

The installer:

1. Checks the Mac: Apple Silicon, macOS 14 or later.
2. Checks **Xcode 16.0 or later**, which provides Swift and the macOS SDK. If Xcode is missing or
   older, it opens Xcode in the App Store and stops; run it again after updating. If only the
   Command Line Tools are active, it switches to Xcode, and it accepts the Xcode license if
   needed. Both of these ask for your password.
3. Installs **Rust** (rustup, Rust 1.88 or later) if it's missing, using Homebrew when available,
   and updates it if it's older.
4. Offers to create a local code-signing certificate (see Signing below).
5. Builds and signs `LanKVM.app`, installs it into `/Applications` (or `~/Applications`), and
   opens it.

`./install.sh --check` only reports what's missing. The minimum versions are at the top of
`install.sh`.

For quick rebuilds while developing, `./scripts/bundle.sh` builds `target/release/LanKVM.app`
without installing it:

```bash
./scripts/bundle.sh && open target/release/LanKVM.app
```

`bundle.sh` builds the Rust core as a static library, links it into the SwiftUI app with
SwiftPM, writes `Info.plist` and signs the bundle. Always launch the `.app`: macOS grants
Screen Recording to the app bundle.

**Signing.** macOS remembers privacy permissions only while the app's signature stays the same.
`bundle.sh` signs with the first "Apple Development" or "lankvm-dev" certificate in your
keychain, or with `$LANKVM_SIGN_IDENTITY`. If neither exists it falls back to ad-hoc signing,
and macOS forgets the permission after every rebuild. To create a free self-signed
certificate (it asks for your login password once):

```bash
./scripts/create-dev-cert.sh
```

## Using it

1. Run LanKVM on every Mac. On a Mac you want to view, open **This Mac**. If screen sharing
   says *Needs permission*, click **Open Privacy Settings**, turn on LanKVM, then **Relaunch
   LanKVM**.
2. **This Mac** shows the address to use, e.g. `192.168.1.20`. On the other Mac, open
   **Connect**, type it and press Return. Each remote Mac opens in its own window.
3. First time only: the viewed Mac shows a 6-digit code. Type it on the viewing Mac. Both Macs
   remember each other after that. **Paired Devices** lists them, with a **Forget** button.
4. The gauge button in the viewer toolbar toggles the latency overlay. Use full screen (⌃⌘F)
   for exact 1:1 pixels.

Set `LANKVM_PORT` to change the UDP port (default 47800).

## Security model

- The app listens on UDP 47800 but only accepts addresses from the local network: RFC 1918,
  link-local, unique-local and loopback.
- Each install has its own certificate (`~/Library/Application Support/lankvm/`). The connection
  is encrypted with TLS 1.3.
- First contact requires the PIN shown on the host. Pairing uses SPAKE2 bound to both
  certificate fingerprints, so a man-in-the-middle can't relay or steal the PIN.
- Trusted devices are stored in `trusted-viewers.txt` and `trusted-hosts.txt` in the same folder.

## Troubleshooting

- **Logs:** `~/Library/Logs/lankvm.log`. Set `RUST_LOG=debug` for more detail.
- **"hasn't allowed Screen Recording":** recent macOS versions never prompt for this and don't
  list the app automatically. In System Settings → Privacy & Security → Screen & System Audio
  Recording, drag LanKVM into the list (or click + and choose it), switch it on, and relaunch
  LanKVM. The This Mac page has buttons for each step.
- **Reset a permission:** `tccutil reset ScreenCapture dev.lankvm.LanKVM`
- **Wi-Fi** adds jitter. For the lowest latency, put the viewed Macs on Ethernet.

## Layout

| Path | What |
|---|---|
| `crates/protocol` | Wire messages, video packet header |
| `crates/transport` | QUIC endpoint, LAN congestion control, identity, pairing, packetizer/reassembler |
| `crates/platform-mac` | ScreenCaptureKit capture, VideoToolbox encode/decode, zero-copy GPU import |
| `crates/core` | Host service, viewer sessions, render thread, C ABI (`ffi.rs`) for the app |
| `macos/` | SwiftUI app (SwiftPM). `Sources/CLanKVM/include/lankvm.h` is the C interface |
| `scripts/bundle.sh` | Builds and signs `LanKVM.app` |
| `scripts/make-icon.swift` | Regenerates `macos/Resources/AppIcon.icns` |

The remote screen never goes through SwiftUI. The viewer window hosts a `CAMetalLayer`, and the
Rust core renders each decoded frame into it from its own thread as soon as it arrives.

Tests: `cargo test --workspace`. This includes a real hardware HEVC encode→decode round trip and
a QUIC loopback session. To review the UI without granting any permissions, render every screen
to PNG (light and dark):

```bash
macos/.build/release/LanKVM --snapshot /tmp/lankvm-shots
```
