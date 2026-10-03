# LanKVM

Low-latency remote desktop for Macs on the same local network, built as a software network KVM.
Every Mac runs the same app. Others on the LAN can view it, and it can connect to them by IP.

**Status:** prototype. You enter an IP, enter a PIN once, and see the other Mac's screen. Each
viewer window has two modes: **View** only looks; **Control** uses the other Mac's keyboard and
mouse as if they were plugged into it.

## How it stays fast

- **Capture → encode on the GPU, no CPU copies.** ScreenCaptureKit delivers IOSurface-backed NV12
  frames straight to the hardware HEVC encoder, which runs flat out instead of pacing itself to
  the frame rate (no B-frames, keyframes only on request). It takes one frame at a time, always
  the newest, so a slow frame costs frame rate, never latency.
- **UDP, never TCP, for video.** One QUIC connection (quinn) carries TLS 1.3, a reliable control
  stream, and unreliable datagrams for video. A fixed-window congestion controller is used
  because the LAN doesn't need Cubic's backoff. Lost frames are dropped, never retransmitted,
  and the viewer requests a fresh keyframe.
- **Decode → display with zero copies.** The hardware decoder outputs IOSurfaces. Each plane is
  wrapped as a Metal texture and sampled by a wgpu shader, then presented immediately with no
  frame queue.
- **The pointer never waits for the video.** While controlling, the remote cursor is drawn
  locally, in the remote Mac's current shape (arrow, I-beam, hand...), and left out of the video,
  so moving the mouse has no lag at all. Clicks and keys travel on their own reliable QUIC stream,
  sent the moment they happen (a backlog of moves is merged, never reordered past a click or a
  key); the viewer asks the host to acknowledge at once, so even a lost packet is resent within a
  few milliseconds. The host injects them from a dedicated high-priority thread.
- **Frames as often as your screen shows them.** The viewer asks for its display's refresh rate:
  120 fps on ProMotion when the stream is small enough to encode that fast (≤ ~5.6 Mpx), 60 fps
  above that, where the encoder sets the pace anyway.
- **Measured, not guessed.** The viewer's latency overlay shows capture→screen latency split into
  capture, encode, network, decode and display, using a clock synced to the host, plus the time
  from your input to its injection on the other Mac.
  `cargo test --release -p platform-mac --test encode_latency -- --ignored --nocapture` measures
  the encoder alone at common screen sizes.

Measured on one M3 Max Mac over loopback: input reaches the host in about 0.3 ms, and posting
it there takes about 2.5 ms. From a click to the changed pixels decoded on the viewer takes, at
the median, 36 ms for a 4112×2658 stream at 60 fps and 25 ms for 2558×1654 at 120 fps.

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

### Controlling the other Mac

- Choose **Control** in the viewer's toolbar, or press **⌃⌥⌘** (Control-Option-Command together,
  then let go). **View** in the toolbar, or **View Only** in the menus, goes back to watching.
  The window remembers the mode per Mac.
- While you control a Mac, every key goes to it, including ⌘Q, ⌘W, ⌘H, Tab and Esc, and, with
  **Control → Send System Shortcuts to Remote Mac** (on by default), also system shortcuts such
  as ⌘Tab and ⌘Space.
- **Release** gives your keyboard and mouse back to your own Mac without giving up control:
  press **⌃⌥⌘**, or click **Release** on the session control. Anything held on the other Mac is
  let go, and in full screen your menu bar and Dock come back. To carry on, click the remote
  screen (that click isn't sent), press ⌃⌥⌘ again, or click **Resume**. Clicking another window
  pauses control the same way, but coming back to the viewer resumes it, unless you released.
- The **session control** is the small tab at the top of the remote screen. Point at it or click
  it to expand it: it says whether you're controlling, with **Release** or **Resume**, and a
  **⋯** menu with Mission Control, App Exposé, Show Desktop and Move Left/Right a Space on the
  other Mac, the input settings, View Only, Exit Full Screen, Keep Expanded, Hide Session
  Control and Disconnect. Drag it by its grip to any edge; double-click the grip to put it back
  at the top. Each Mac's window remembers where it was. Moves, clicks and scrolling over it stay
  on your Mac (keys still go to the other one). It shows while you control; while you only
  watch, it shows in full screen, where the toolbar is hidden. The **Control** menu has the same
  actions, and **Show Session Control** brings it back after you hide it.
- In full screen while controlling, your own menu bar and Dock stay hidden, so the screen edges
  reach the other Mac's menu bar, Dock and hot corners. Release (⌃⌥⌘, or the session control)
  to get them back.
- Clicks in the black bars around the picture land on the nearest edge of the other Mac's
  screen (within 24 pt; further out they're ignored). Scrolling keeps trackpad momentum and your
  natural-scrolling direction. Keys are sent by position, so the other Mac's keyboard layout and
  input method decide the characters (dead keys and IME work there).
- **Trackpad gestures** go to the other Mac too (**Control → Send Trackpad Gestures to Remote
  Mac**, on by default). Pinch, rotate, smart zoom and swipes between pages act on what's under
  the pointer there. Swiping between Spaces, Mission Control, App Exposé and the pinches for
  Show Desktop and Launchpad (Apps) also need Accessibility for LanKVM on **your** Mac, the same
  switch as for being controlled. Without it they act on your Mac, and the viewer offers **Open
  Settings**. They go to the other Mac when they start over its picture, or anywhere on the
  screen in full screen; elsewhere, or once you release (⌃⌥⌘), they're your Mac's again. Mission
  Control, App Exposé, Show Desktop and Move Left/Right a Space are also in the session control's
  **⋯** menu and in **Control → Remote Mac**.
- Not possible over the network: Force Touch and Look Up, the swipe in from the right edge for
  Notification Center, the Globe key's system actions, media keys, and the login window.

**On the Mac being controlled**, macOS must allow LanKVM to post input: **This Mac → Remote
control → Allow Control…**, then switch LanKVM on under Privacy & Security → Accessibility. The
same card has **Let paired Macs control this Mac** (on by default). While anyone is connected, a
menu-bar icon lists them, with **Stop Control** and **Disconnect**; **⌃⌥⌘.** (period) also takes
control back. One Mac controls at a time: another device has to choose to take over. Two Macs
can't control each other at once: when the Mac you're controlling takes control of yours, your
window on it switches to View (otherwise every key would bounce between the two).

### Working on a big screen: virtual displays

The Mac with the big monitor can be the other Mac's display. The other Mac makes a display that
exists only in software, exactly the size of your screen, and LanKVM shows it pixel for pixel: a
MacBook Pro whose own screen is 3456 × 2234 gets a 6144 × 2560 Retina display, say, which looks
like 3072 × 1280.

1. Connect, then choose **Display → This Screen's Size** in the viewer's toolbar (also in the
   session control's **⋯** menu and the **Control** menu). Other screens of your Mac are listed
   too. **Custom Size…** takes any size from 640 × 480 to 8K (8192 × 4320, at most 4:1);
   **Retina** draws it at 2x, so it looks like half that.
2. Go full screen (⌃⌘F) on that monitor: one pixel of the other Mac is one pixel of yours.
3. Choose how the display sits on the other Mac:
   - **Only It** (the default): its own screens show a copy, and every window is on the new
     display.
   - **As Main Display**: the menu bar, Dock and new windows go to it; its own screen stays
     separate.
   - **Next to *Mac*'s Screen**: an extra display; windows stay where they are.

The window remembers the choice per Mac and sets it up again when you reconnect. **Its Own
Screen** goes back.

- The display goes away when you disconnect, and the other Mac's windows come back to its own
  screen. If the connection drops, it stays for a minute, so reconnecting finds every window
  where it was.
- On the other Mac, **This Mac** lists the displays other Macs added, with **Remove**; the
  menu-bar icon has **Use This Mac's Own Screen**, and **⌃⌥⌘.** removes them too. A paired Mac
  can add one only while **Let paired Macs control this Mac** is on (turning it off removes them),
  and only **Next to *Mac*'s Screen** while another Mac controls it. Switching to another user
  removes them too, and none can be added until you're back.
- Keep a MacBook's lid open: a Mac whose only other display is virtual sleeps when the lid closes.
  Turn its brightness down instead.
- 6144 × 2560 encodes in about 16 ms on an M3 Max, so it streams at up to 60 fps, at about
  113 Mbit/s: use Ethernet. A Mac makes at most two virtual displays at once.
- Both Macs need this version of LanKVM. Virtual displays use a private macOS interface (as
  BetterDisplay and DeskPad do); LanKVM checks it's there, exactly as expected, before using it.

Set `LANKVM_PORT` to change the UDP port (default 47800).

## Testing a connection on one Mac

Connecting LanKVM to itself isn't a real test: one process plays both roles with one identity.
Run two separate devices instead. `LANKVM_DATA_DIR` gives an instance its own certificate,
trust lists and log, and `LANKVM_PORT` its own port.

**Two apps.** With the normal LanKVM running, start a second one, "LanKVM 2", on port 47801:

```bash
./scripts/second-instance.sh
```

In LanKVM 2, connect to `127.0.0.1:47800` and type the PIN that LanKVM shows. LanKVM 2 is a
copy with its own bundle id, so macOS keeps its permissions separate, as on another Mac. It
only needs Screen Recording if you view it from the first instance. Its log is in
`~/Library/Application Support/lankvm-2/lankvm.log`.

**Headless viewer.** For a repeatable check with numbers, the probe connects, pairs, decodes
the host's screen and prints the viewer's stats every second. It exits 0 only if frames were
decoded:

```bash
cargo run --release -p lankvm-core --example probe -- 127.0.0.1:47800 --seconds 10
```

The first run asks for the PIN shown on the host. Later runs reconnect without one (the probe
keeps its identity in `~/Library/Application Support/lankvm-probe`). `--virtual 6144x2560@2x`
asks the host for a virtual display and passes only if the decoded frames have that size; it is
arranged next to the host's screen unless `--arrange main` or `--arrange only` says otherwise. `--max 1920x1200` limits
the stream size, like a viewer with a smaller screen. Frames are only sent when the host's
screen changes, so keep something moving there when you measure fps.

Both run over loopback, so they test everything except a real network: Wi-Fi jitter and loss,
the firewall, and macOS's Local Network permission. Check those with a second Mac.

**Remote control on one Mac.** `scripts/e2e-control.sh` runs a test host (port 47810), viewer
(47811) and headless probe as three pre-paired devices, next to (and without touching) the LanKVM
you use on 47800. **LanKVM Input Lab** (`scripts/input-lab.swift`) is the target app: it logs
every event it receives and flips a big square black/white on each click, for latency tests.

```bash
./scripts/e2e-control.sh setup          # build, create identities, pre-pair
./scripts/e2e-control.sh lab            # start Input Lab
./scripts/e2e-control.sh host hid       # host that really injects (only into Input Lab)
./scripts/e2e-control.sh probe --control --lab target/e2e/input-lab.jsonl --script scripts/e2e/basic.jsonl
python scripts/e2e/check_lab.py target/e2e/input-lab.jsonl
./scripts/e2e-control.sh probe --control --lab target/e2e/input-lab.jsonl --input-latency 30
./scripts/e2e-control.sh stop
```

`host record` injects nothing and writes the events it would post to `target/e2e/host/injected.jsonl`
instead (no permission needed). `host hid` needs Accessibility for LanKVM and moves the real mouse,
so leave the Mac alone while it runs; a guard drops anything that wouldn't land in Input Lab,
and control ends by itself after two minutes. `scripts/e2e/` has scenarios for typing, clicks,
drags, scrolling, a viewer that goes silent while holding a key, and a disconnect mid-press.
`./scripts/e2e-control.sh viewer` starts LanKVM 2 against the test host for trying the real
viewer window (on one Mac the pointer and keyboard are shared, so the viewer ignores the input
its own host injects).

`cargo test --workspace` covers the control path without any permission: two cores talk over
real QUIC on loopback while the host records what it would inject (`crates/core/tests/control_loopback.rs`).

## Security model

- The app listens on UDP 47800 but only accepts addresses from the local network: RFC 1918,
  link-local, unique-local and loopback.
- Each install has its own certificate (`~/Library/Application Support/lankvm/`). The connection
  is encrypted with TLS 1.3.
- First contact requires the PIN shown on the host. Pairing uses SPAKE2 bound to both
  certificate fingerprints, so a man-in-the-middle can't relay or steal the PIN. One device
  pairs at a time, and after 5 tries without success pairing pauses for 30 s, doubling each
  time up to an hour, so the 6-digit PIN can't be guessed by brute force.
- Trusted devices are stored in `trusted-viewers.txt` and `trusted-hosts.txt` in the same folder.
- Adding a virtual display (and making it the main display, or mirroring the host's screens to it)
  needs **Let paired Macs control this Mac**, but not Accessibility: it moves the host user's
  windows, so it counts as control. The host user can always remove it (This Mac, the menu-bar
  icon, **⌃⌥⌘.**); quitting or crashing removes it too, and macOS puts the screens back.
- Only paired devices can view, and controlling needs more: the host's **Let paired Macs control
  this Mac** setting (stored in the data folder) and macOS Accessibility permission for LanKVM on
  the host. One device controls at a time. Input is ignored unless granted,
  checked and clamped (key codes, buttons, scroll values), and a flood of presses ends control.
  A viewer that floods control requests or stops reading what the host sends is disconnected.
- Every event a host injects carries a per-process tag, so a viewer on the same Mac drops it
  instead of sending it back, plus how many Macs it has been relayed through (A controls B,
  whose window controls C): hosts drop input that went around more than three, so Macs
  controlling each other in a loop can't bounce it around forever. Controlling a Mac from itself is refused unless the host runs with
  `LANKVM_ALLOW_SAME_MAC_CONTROL=1` (testing only).

## Troubleshooting

- **Logs:** `~/Library/Logs/lankvm.log`. Set `RUST_LOG=debug` for more detail.
- **"hasn't allowed Screen Recording":** recent macOS versions never prompt for this and don't
  list the app automatically. In System Settings → Privacy & Security → Screen & System Audio
  Recording, drag LanKVM into the list (or click + and choose it), switch it on, and relaunch
  LanKVM. The This Mac page has buttons for each step.
- **"hasn't allowed LanKVM to control it":** on that Mac, open This Mac → Remote control →
  Allow Control…, and switch LanKVM on under Privacy & Security → Accessibility. If it's already
  on but control still fails, remove it with −, add it again, and relaunch LanKVM.
- **Spaces and Mission Control swipes act on your own Mac while controlling:** LanKVM needs
  Accessibility on the Mac you control *from* too (Privacy & Security → Accessibility). Pinch,
  rotate and page swipes work without it.
- **Reset a permission:** `tccutil reset ScreenCapture dev.lankvm.LanKVM` (or `Accessibility`)
- **Wi-Fi** adds jitter. For the lowest latency, put the viewed Macs on Ethernet.
- **A virtual display looks soft:** go full screen (⌃⌘F) on the screen whose size it matches; in a
  window the picture is scaled.
- **Windows are on a display you can't see** (on the Mac that made a virtual display): click
  **Use This Mac's Own Screen** in LanKVM's menu-bar icon, press **⌃⌥⌘.**, or quit LanKVM.
- `cargo test -p platform-mac --test virtual_display -- --ignored` creates a virtual display for a
  second (next to your screen) to check this macOS still supports them.

## Layout

| Path | What |
|---|---|
| `crates/protocol` | Wire messages, video packet header |
| `crates/transport` | QUIC endpoint, LAN congestion control, identity, pairing, packetizer/reassembler |
| `crates/platform-mac` | ScreenCaptureKit capture, VideoToolbox encode/decode, zero-copy GPU import, input injection (`inject.rs`, `keys.rs`), cursor shapes (`cursor.rs`), virtual displays (`virtual_display.rs`) |
| `crates/core` | Host service, viewer sessions, remote control (`control.rs`), virtual displays for viewers (`displays.rs`), render thread, C ABI (`ffi.rs`) for the app |
| `macos/` | SwiftUI app (SwiftPM). `Sources/CLanKVM/include/lankvm.h` is the C interface |
| `scripts/bundle.sh` | Builds and signs `LanKVM.app` |
| `scripts/make-icon.swift` | Regenerates `macos/Resources/AppIcon.icns` |
| `scripts/e2e-control.sh`, `scripts/e2e/`, `scripts/input-lab.swift` | One-Mac remote-control tests |

The remote screen never goes through SwiftUI. The viewer window hosts a `CAMetalLayer`, and the
Rust core renders each decoded frame into it from its own thread as soon as it arrives.

Tests: `cargo test --workspace`. This includes a real hardware HEVC encode→decode round trip, a
QUIC loopback session, and remote control end to end over QUIC. To review the UI without granting any permissions, render every screen
to PNG (light and dark):

```bash
macos/.build/release/LanKVM --snapshot /tmp/lankvm-shots
```
