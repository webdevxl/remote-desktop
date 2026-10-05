# LanKVM

Low-latency remote desktop for Macs on the same local network, built as a software network KVM.
Every Mac runs the same app. Others on the LAN can view it, and it can connect to them by IP.
Paired Macs can also connect over the internet, if the Mac they connect to turns that on, with
no router setup: a small LanKVM server introduces them to each other, or, with no server at all,
the BitTorrent DHT (the network torrent apps find each other through).

**Status:** prototype. You enter an IP, enter a PIN once, and see the other Mac's screen. Each
viewer window has two modes: **View** only looks; **Control** uses the other Mac's keyboard and
mouse as if they were plugged into it, and shares the clipboard with it.

## How it stays fast

- **Only what changed is encoded, in tiles, all at once.** The picture is split into tiles (a
  6144×2560 screen into 16 of 3072×320), and each tile is its own video stream with its own
  hardware HEVC encoder. Each captured frame is compared with what every tile last sent
  (ScreenCaptureKit's dirty rectangles say which tiles to look at; every tile is compared four
  times a second anyway), and only tiles that changed are encoded: typing or a menu re-encodes a
  sliver of the screen in 2-5 ms instead of the whole picture in 17 ms. The changed tiles are
  encoded together and each goes on the wire the moment it's done. When most of the screen
  changes (scrolling, a new window), one more encoder sends the whole picture instead, which the
  media engine does faster than many tiles. The encoders run flat out (no B-frames, keyframes
  only on request) and take one frame at a time, always the newest, so a slow frame costs frame
  rate, never latency.
- **UDP, never TCP, for video.** One QUIC connection (quinn) carries TLS 1.3, a reliable control
  stream, and unreliable datagrams for video. A fixed-window congestion controller is used
  because the LAN doesn't need Cubic's backoff. Over the internet the window paces packets at a
  few times the video's bitrate, so a frame leaves in a fraction of a frame interval, and the
  bitrate itself backs off on loss and queueing (Cubic spread each frame over most of a round
  trip, and shrank on every random loss). Lost frames are dropped, never retransmitted: the
  viewer asks for a keyframe of just the tile that lost one (1/16 of a whole keyframe), and
  knows from every frame which tiles its update had, so even a tile lost whole on a still screen
  is asked for again. Each side sends a tiny packet every 20 ms when idle, so a Wi-Fi radio
  never dozes off between clicks.
- **Decode → display with zero copies.** Each tile has its own hardware decoder, decoding as soon
  as the tile arrives, in parallel with the others. The render thread copies decoded tiles into a
  picture on the GPU with Metal and draws it as soon as every tile of a frame is in, so a frame
  is never shown half old, half new. No frame queue anywhere.
- **The pointer never waits for the video.** While controlling, the remote cursor is drawn
  locally, in the remote Mac's current shape (arrow, I-beam, hand...), and left out of the video,
  so moving the mouse has no lag at all. Clicks and keys travel on their own reliable QUIC stream,
  sent the moment they happen (a backlog of moves is merged, never reordered past a click or a
  key); the viewer asks the host to acknowledge at once, so even a lost packet is resent within a
  few milliseconds. The host injects them from a dedicated high-priority thread.
- **Frames as often as your screen shows them.** The viewer asks for its display's refresh rate,
  up to 120 fps at any size: only changed tiles are encoded, so even a 6K display keeps up with
  typing and moving windows at 120 Hz. The bit budget stays at 60 fps worth, spread over more,
  smaller frames.
- **Measured, not guessed.** The viewer's latency overlay shows capture→screen latency split into
  capture, encode, network, decode and display, using a clock synced to the host and the moment
  Metal reports the frame on screen, plus the time from your input to its injection on the other
  Mac. Benchmarks (`cargo test --release -p platform-mac --test <name> -- --ignored --nocapture`):
  `encode_latency` (one encoder at common screen sizes), `stripe_encode` (tiles against one
  encoder, and the full frame), `tiler` (comparing and copying tiles).

Measured on one M3 Max Mac (6144×2560, nothing else encoding): one changed tile is out of the
encoder 3.6-5.4 ms after the frame is taken, against 17 ms when every frame was encoded whole; a
whole-screen change takes 19 ms as one full frame (27 ms as 16 tiles, which is why big changes go
whole). Input reaches the host in about 0.3 ms, and posting it there takes about 2.5 ms.

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
SwiftPM, puts the LanKVM Microphone driver inside (see Using your microphone on the other Mac),
writes `Info.plist` and signs the bundle. Always launch the `.app`: macOS grants Screen Recording
and the microphone to the app bundle.

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
4. From then on the Mac is under **Your Macs** in **Connect**: click **Connect** next to it (see
   below).
5. The gauge button in the viewer toolbar toggles the latency overlay. Use full screen (⌃⌘F)
   for exact 1:1 pixels.

### Your Macs: names, addresses and how to connect

**Connect** lists every Mac you paired with under **Your Macs**, with its two addresses:

- **Local**: where this Mac last reached it on the local network (or else where it said it is
  there). *Not known yet* until this Mac has reached it there: type its address once.
- **Internet**: where it was last reached, or said to reach it, over the internet. *Through the
  LanKVM server* when only the server knows; *Not set up* while its internet access is off (see
  Connecting over the internet).

The switch next to each picks how **Connect** reaches that Mac, and is remembered per Mac:

- **Auto** (the default) tries every way at once, the local network and the internet, and keeps
  the local network when both answer.
- **Local** connects only on the local network, at its local address.
- **Internet** connects only over the internet: at its internet address, and through the LanKVM
  server. It stays there even when the Mac turns out to be on your network, so it is also the
  way to try the internet path from home.

**Connect** is greyed out while the chosen way has no address; hover over it to see what's
missing. An address typed at the top always goes where it says, whatever the switch.

Give a Mac a name of your own with the pencil next to its name (or right-click the row, or
**Rename** in **Paired Devices**): e.g. *Office* for a Mac that calls itself *Mac mini*. LanKVM
on this Mac then calls it that everywhere, the viewer window and its messages included. Leave
the name empty to go back to its own. Names, addresses and the choice stay on this Mac, in
`address-book.json` in its data directory, and go when you forget the Mac. A Mac in **Your Macs**
isn't repeated under **Recent**.

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

### Copy and paste between the Macs

While you control another Mac, the two share one clipboard: copy on either, paste on the other.
Text, rich text, web content (HTML), links, images and PDFs go across. Files stay where they are:
copying one empties the other Mac's clipboard, so it never pastes something older.

- **Control → Share Clipboard** (also in the session control's **⋯** menu) turns it off or on for
  every Mac this one controls. It's on by default. While it's off, or while you only view a Mac,
  neither clipboard leaves its Mac.
- The newest copy wins. When you start controlling a Mac, whichever clipboard was copied last, on
  either Mac, goes to the other. After that each copy goes across within a quarter of a second,
  and at once when you switch between the Macs (⌃⌥⌘, or clicking the remote screen or another
  window), just before you paste.
- A chain shares one clipboard: when your Mac controls one whose window controls a third, a copy
  on any of them reaches the others.
- Up to 64 MB over the local network, 8 MB over the internet. When a clipboard is bigger, the
  viewer window says so and the other Mac's clipboard is emptied instead. An image copied as TIFF
  goes as PNG, a fraction of the size.
- A password copied from a password manager keeps its "concealed" mark (nspasteboard.org), so
  clipboard history apps on the other Mac leave it out too.
- LanKVM checks only whether the clipboard changed (a counter macOS keeps), a few times a second,
  and reads it only while it shares it. If macOS asks whether LanKVM may paste from other apps,
  allow it.

### Using your microphone on the other Mac

The Mac you sit at can lend its microphone to the Mac you view or control: apps there (a call in
Zoom, Teams or FaceTime, a recording, dictation) choose **LanKVM Microphone** as their
microphone and hear you.

1. **Once, on the other Mac:** open **This Mac → Microphone** and click **Install…**. That installs
   LanKVM Microphone, a small audio driver, into `/Library/Audio/Plug-Ins/HAL`: macOS asks for an
   administrator password, and the Mac's sound restarts for a moment. (`./scripts/install-audio-driver.sh`
   does the same from Terminal.) **Let paired Macs control this Mac** must be on there: a
   microphone is input too.
2. **On your Mac:** click the microphone button in the viewer's toolbar, or choose **Share
   Microphone** (session control's **⋯** menu, or the **Control** menu). The first time, macOS
   asks whether LanKVM may use the microphone.
3. **On the other Mac:** pick **LanKVM Microphone** as the input in the app (or in System Settings
   → Sound → Input, for every app).

- Your Mac's current microphone goes (its default input in Sound settings), and LanKVM follows
  when you change it, a headset plugged in say. Only the Mac whose window shares it hears it: each
  window has its own switch, off every time it connects.
- Several Macs can share their microphones with one Mac at once: LanKVM Microphone plays them
  mixed. This Mac lists whose microphone is on, and so does the menu-bar icon. Turning off **Let
  paired Macs control this Mac** stops them.
- Audio goes as 48 kHz, 16-bit mono in 10 ms packets (about 0.8 Mbit/s), on QUIC datagrams like
  the video: a lost packet is a 10 ms gap rather than a growing delay. The other Mac keeps 20 ms
  of it in hand on the local network (40 ms over the internet), more for a while after the
  network stalled, and follows the two Macs' clocks drifting apart without a sound.
- Without the microphone permission macOS sends LanKVM silence: allow it in System Settings →
  Privacy & Security → Microphone. LanKVM never sends LanKVM Microphone itself (a Mac whose own
  input is set to it), which would play other Macs' audio back to them.
- **Remove…** under This Mac → Microphone uninstalls the driver (`./scripts/install-audio-driver.sh
  --remove` from Terminal). **Update…** shows there when this LanKVM has a newer driver.

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
- 6144 × 2560 streams at up to 120 fps: a change to part of it encodes in a few milliseconds, a
  change of the whole screen in about 17 ms on an M3 Max. Big changes take up to about
  113 Mbit/s: use Ethernet. A Mac makes at most two virtual displays at once.
- Both Macs need this version of LanKVM. Virtual displays use a private macOS interface (as
  BetterDisplay and DeskPad do); LanKVM checks it's there, exactly as expected, before using it.

Set `LANKVM_PORT` to change the UDP port (default 47800). On the viewed Mac, `LANKVM_TILES=COLSxROWS`
forces the tile grid (`1x1` encodes the whole picture as one stream, as older versions did), and
`LANKVM_FULL_FRAME_AT` (default 0.6) is the share of the picture that has to change for it to go
as one full frame (above 1: never).

### Connecting over the internet

Off by default, and only for Macs that have paired. Nothing to set up on either router.

1. **Pair on the same network first.** Connect once from the other Mac on your network and enter
   the code. Pairing never happens over the internet. On every connection the host also hands
   the viewer its key for the internet, and says how to reach it there (see Security model).
2. On the Mac to reach, open **This Mac** and turn on **Let paired Macs connect over the
   internet**. It registers with the LanKVM server (`178.156.129.211:3478`, UDP) and stays in
   touch with it, which also keeps its router open for it. The card says *Reachable from
   anywhere through the LanKVM server* once it is.
3. Connect from the other Mac once more on the local network while internet access is on there,
   so it learns the way. From then on, click **Connect** next to the Mac under **Your Macs**
   (with **Auto** or **Internet**), from anywhere.

The server only introduces the two Macs. Each sends a few packets straight at the other's public
address, which opens both routers, and the session then runs directly from Mac to Mac. Where
routers won't let a direct path through (some give every destination its own outside port, and
carrier-grade NAT or strict firewalls block it), the server passes the session's packets along
instead: still encrypted end to end, so the server can't read them, and the viewer window says
*Internet · relayed*. A direct path has the lower latency.

Two Macs on one network connect there, even when one is told the other's public address or
goes through the server (unless you chose **Internet** for that Mac): the host says where it is on its network every time a viewer connects,
and the viewer tries that too, at the same time, and prefers it when it is on the viewer's own
network. Routers rarely send traffic for their own public address back inside, so without that
the session would go out to the LanKVM server and back. A session that went through the relay anyway (the first time, before
the viewer knew that address) moves onto the local network a moment later, without starting over.

The **LanKVM server** field under internet access on This Mac picks another server (`host:port`),
or none when left empty: then paired Macs reach the Mac through the BitTorrent DHT (below), or
directly through its router. It has to be on the internet (paired Macs don't use one on a local
network), and changing it ends the sessions relayed through the old one. The server is the
`lankvm-rendezvous` service in the hivex-crm repository.

**With no server at all: the BitTorrent DHT.** Torrent apps find each other with no tracker
through the "Mainline" DHT: millions of computers running torrent apps, each keeping a little of
a shared table. LanKVM uses it the same way, so two paired Macs meet with no LanKVM server
(**Find this Mac through the BitTorrent DHT** under internet access on This Mac, on by default;
the same switch decides whether this Mac looks there as a viewer). A viewer looks there for a
Mac that had internet access on when they last connected on the local network, as it does for
the LanKVM server:

1. The Mac to reach leaves a note on the DHT for each paired Mac: where it is (its public address
   as DHT nodes see it, its IPv6 address, its address on its own network) and which DHT nodes it
   watches for requests. The note is signed and encrypted with a key only the two Macs have (made
   from the access key), so the nodes storing it learn nothing, and nobody else can write one the
   other Mac accepts. Its place on the DHT changes every day.
2. The other Mac finds the note (one lookup, usually a second or two), leaves a request with its
   own addresses at the nodes the note names, and sends a few packets toward the host. Where the
   host can be reached without punching (its router forwards the port, an IPv6 firewall lets it
   in, or both Macs are on one network), it connects right away, in about a second.
3. The host checks those nodes every 2 seconds. When the request comes, it sends a few packets
   straight at the viewer, which opens its router for the viewer's packets, and answers in its
   note. The viewer connects the moment the host's packets arrive (they also say where the host
   is), or when the answer appears: usually 3–6 seconds in all (measured from a home connection
   on the public DHT), up to about 9 just after the host restarted, against well under a second
   through the LanKVM server, which is why both are tried at once when a server is set.

From then on the session is the same as through the server's introduction: straight from Mac to
Mac, the same latency, knock and TLS included. What the DHT can't do is relay: where neither
router lets a direct path through (one that gives every destination its own outside port, or
carrier-grade NAT), only the LanKVM server's relay gets through over IPv4. Over IPv6 there is no
address translation to get around, only the firewalls in front of each Mac, which the same packets
open, so two Macs with IPv6 (most home connections, many phone networks) connect directly even
there. This Mac's card shows where DHT nodes see it, and warns when its router changes ports.

LanKVM joins the DHT through a few well-known nodes (`dht.libtorrent.org`,
`dht.transmissionbt.com`, BitTorrent's own) the first time, then through the nodes it met last
time (`dht-nodes.txt` in its data directory). It never answers DHT queries (it joins read-only, as
BEP 43 allows), so its port still looks closed to strangers; while internet access is on, the host
sends a few small packets every 2 seconds per paired Mac.

**From the terminal.** `cargo run --release -p transport --example dht -- check` joins the DHT,
says where DHT nodes see this Mac and whether its router lets punching through, and checks that a
second client finds what it stores there. Two Macs with nothing but the terminal, the DHT and no
server (build once with `cargo build --release -p lankvm-core --examples`):

```bash
target/release/examples/probe host --no-server
```

on the Mac to reach (a headless host: it prints its fingerprint, the code to pair with, and how
it does on the DHT; `--no-video` if the terminal hasn't got Screen Recording permission), then on
the other Mac, once on the same network to pair (the address the host printed):

```bash
target/release/examples/probe 192.168.1.20:47800
```

and from anywhere after that, through the DHT and no other way:

```bash
target/release/examples/probe lankvm:<fingerprint> --only-dht
```

**A direct path through the router (optional).** LanKVM also asks the router to forward its UDP
port (macOS speaks UPnP, NAT-PMP and PCP for it). When the router does, the card shows the
address, e.g. `203.0.113.7:47800`, and paired Macs try it along with the server. When it doesn't,
the card says what to forward by hand: UDP port 47800 to this Mac's address on the network, e.g.
`192.168.1.20`. Reserve that address for the Mac in the router's settings (a DHCP reservation),
so the forward keeps pointing at it. Most home connections get a new public address now and then:
if you have a dynamic DNS name, or forwarded the port by hand, enter it under **Public address**,
e.g. `home.example.com` (the port is added if you leave it out). The host tells its paired Macs
where to reach it each time they connect. Once they know it at an address, they don't knock
anywhere else (see Security model). You can also type the address in **Connect**.

Two Macs behind one router can both be reachable: the router gives the second one another
outside port, and its card shows the address with that port, e.g. `203.0.113.7:47801`. (Or
start it with its own `LANKVM_PORT`.) Turning the setting off removes the forward, takes the Mac
off the LanKVM server and ends the sessions that came over the internet; sessions on the local
network carry on.

Over the internet the picture follows the connection: changes go out as tiles only, at up to
60 fps, and the bitrate starts at 12 Mbit/s and rises as far as the connection allows, backing off
when packets get lost or start to queue (through the LanKVM server's relay, to at most 32 Mbit/s,
under what the server passes). On a slow connection the picture gets softer rather than falling
behind: the bitrate comes down to what the connection delivers. What limits it is usually the upload of the Mac you're looking at: for the
best picture, give that Mac a fast upload.

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

**The shared clipboard on one Mac.** Two copies of LanKVM on the same Mac have the same
clipboard, so they don't share it. `LANKVM_CLIPBOARD=<name>` gives an instance a pasteboard of its
own instead (`off`: none), which it then shares; `crates/core/tests/clipboard_loopback.rs` runs two
cores that way, never touching your clipboard.

**The microphone on one Mac.** `LANKVM_MICROPHONE=tone` makes an instance send a 440 Hz tone
(`tone:1000`: another pitch) instead of its microphone, and `off` none; `./scripts/second-instance.sh`
passes it on. `crates/core/tests/microphone_loopback.rs` runs two cores with a tone and a recording
in place of LanKVM Microphone, never touching your audio devices, and `./scripts/build-audio-driver.sh`
checks the driver itself by loading it as macOS's audio system does, without installing it. With the
driver installed, `probe HOST --microphone --seconds 30` sends the tone to a host for real.

**Internet access on one Mac.** `LANKVM_TEST_LOOPBACK_IS_INTERNET=1` makes an instance treat
loopback as the internet, so its connections over `127.0.0.1` go through the gate, the knock and
the internet checks; the router is left alone, and so is the LanKVM server unless
`LANKVM_RENDEZVOUS` names one. Pair the two instances without it first (pairing never runs over
the internet), then restart both with it. `crates/core/tests/internet_loopback.rs` does the same
in `cargo test`, with an in-process LanKVM server (`transport::test_server`) for connecting
through a server: punched, relayed, and when that server lies about where each Mac is.

`LANKVM_RENDEZVOUS=host:port` sets the LanKVM server an instance registers with, in place of the
setting (`LANKVM_RENDEZVOUS=` with nothing: none). `LANKVM_TEST_FORCE_RELAY=1` makes a viewer
reach paired Macs (Connect in Paired Devices) through their server's relay alone, without trying
a direct path.

**The viewer window, measured from outside.** `scripts/viewer-bench.sh [MODE [LABEL]]` runs the
host copy, a virtual display animated by Frame Source and the GUI viewer copy (LanKVM 2), and
films the viewer window with LanKVM's screen scope (the app run with `LANKVM_SCOPE_LOG`, for its
Screen Recording permission), reading each frame's number off the glass: source drawn → on this
screen, and distinct frames shown per second. `python3 scripts/viewer-report.py
target/e2e/bench/RUN...` puts runs side by side. On one Mac the viewer shares the GPU with the
host's content, so the source draws fewer frames than with the headless probe. Leave the Mac alone
and unlocked while it runs.

## Security model

- The app listens on UDP 47800. It answers devices on the local network (RFC 1918, link-local,
  unique-local and loopback), and nobody else unless internet access is on. A session from the
  local network ends if the viewer takes it onto the internet (QUIC would follow it there).
- **Over the internet only paired devices get an answer**, and only while the host has internet
  access on. A gate in front of the QUIC socket looks at every packet from outside the local
  network. A paired viewer's first packet carries a knock in its connection ID: a random nonce
  and a tag over it made with that viewer's access key and the current 10-minute period of the
  clock. Packets without a valid, fresh knock are dropped before QUIC sees them, so a stranger
  gets no reply at all (no version negotiation, retry, close or reset): the port looks closed.
  A knock copied off the wire is ignored from any other address, and how many knocks are
  checked per second is limited. A viewer knocks at an address only with the keys of hosts it
  reached or was told about there (or that haven't said where they are yet), so a wrong address
  doesn't get knocks it could pass on to the viewer's other hosts.
- Each viewer's access key is derived from a secret only the host has (`internet-secret.key`,
  readable only by its user) and the viewer's certificate fingerprint, and is handed over on the
  local network. The viewer keeps it in `internet-hosts.json` (likewise private). After the
  knock, TLS still has to prove the device holds the certificate the key was made for, and the
  device has to be paired.
- **The LanKVM server** knows each registered host by an ID derived from a key only that host has
  (`rendezvous-key.p8`, private): nobody else can register under its ID or move it elsewhere. A
  viewer asks for an introduction with a token made from its access key, for that request and the
  current 10-minute period; only the host checks it, and it answers nothing that doesn't check
  out (nor a copy from another address). So a stranger who learns a host's ID gets no answer from
  it, through the server or otherwise. What the server learns: the public addresses of the Macs
  that use it, their IDs, when they connect, and how much traffic it relays. It can't read or
  alter a session (TLS 1.3 end to end, with the certificates pinned) or pose as either Mac. The
  worst it can do is refuse its service, or introduce a Mac to the wrong address, where the
  session fails like any wrong address would.
- **The BitTorrent DHT** stores a host's notes and its viewers' requests, a few hundred bytes each,
  sealed (ChaCha20-Poly1305) and signed (Ed25519) with keys derived from the pair's access key.
  The nodes storing them see the public addresses of the Macs that store and fetch them, and when,
  but not what they say or which Macs they belong to; the keys, and so the notes' places on the
  DHT, change every day. A note or request that doesn't open with the pair's key is ignored, as is
  a request older than 10 minutes or seen before. The worst a node can do is keep a note back,
  or serve an older one, and the connection fails or waits like through a server that refuses.
  Notes are kept only on nodes whose ID follows from their IPv4 address (BEP 42), so sitting
  where a pair's notes go takes control of many addresses, not just of IDs. The host only sends a few packets toward the addresses
  in a genuine request, and they carry a value only the pair can make, so the viewer connects
  only where the host really is; everything after that is the knock and TLS as above. Where DHT
  nodes see the host is only ever told to its viewers inside its sealed notes: strangers say
  it, so viewers don't keep it as an address to knock at. LanKVM never sends to an address a
  DHT node names on a local network (loopback, private, link-local).
- **Forget** revokes a viewer's key at once and ends its sessions. Turning internet access off
  stops all knocks and ends every session that came over the internet.
- Pairing only happens on the local network: a host never shows a code for a connection from
  the internet.
- What a stranger scanning the port sees: nothing. One answer can still get out: QUIC's
  stateless reset for a packet addressed to a connection ID the host issued earlier. Only
  someone who watched an earlier session's packets has one, and all it tells them is that
  LanKVM still runs there.
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
- **The clipboard** goes only between a Mac and the paired Mac controlling it, while that Mac's
  user has **Share Clipboard** on, inside the session's encrypted connection. A Mac writes only
  the shared types (text, rich text, HTML, links, PNG, PDF and the nspasteboard.org marks) that
  come from the other: never files or app-private types. The host also accepts clipboard
  transfers only from the viewer controlling it, and reads at most two at a time.
- Every event a host injects carries a per-process tag, so a viewer on the same Mac drops it
  instead of sending it back, plus how many Macs it has been relayed through (A controls B,
  whose window controls C): hosts drop input that went around more than three, so Macs
  controlling each other in a loop can't bounce it around forever. Controlling a Mac from itself is refused unless the host runs with
  `LANKVM_ALLOW_SAME_MAC_CONTROL=1` (testing only).

## Troubleshooting

- **Logs:** `~/Library/Logs/lankvm.log`. Set `RUST_LOG=debug` for more detail.
- **"… needs LanKVM Microphone first":** install it on the Mac you view (This Mac → Microphone).
  If This Mac says *Not loaded*, click **Restart Audio…** or restart that Mac.
  `system_profiler SPAudioDataType | grep -A3 "LanKVM Microphone"` shows whether macOS has it.
- **The other Mac hears silence:** on your Mac, allow LanKVM under Privacy & Security →
  Microphone, and check the input level of your microphone in Sound settings.
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
- **"… isn't reachable over the internet right now":** the LanKVM server doesn't know that Mac:
  LanKVM isn't running there, or its internet access is off. **"Couldn't reach the LanKVM server
  at …":** this Mac's network blocks it (UDP 3478), or the server is down. **"… didn't answer
  through the LanKVM server":** that Mac no longer takes this one's key (it forgot this Mac:
  connect on the local network once more), or the two clocks differ by over 10 minutes.
- **"No answer from …" over the internet:** on the Mac you connect to, look at This Mac →
  Internet access. *The router didn't answer* or *can't open ports automatically*: turn on UPnP
  or NAT-PMP in the router's settings, or forward UDP port 47800 to the address shown yourself.
  *Behind another router* (double NAT, e.g. a provider's modem-router in front of your own):
  forward the port on the outer router too, or put one of them in bridge mode. *Shares one public
  address* (carrier-grade NAT): the internet can't reach the Mac at all; ask the provider for a
  public IPv4 address, or use a VPN.
- **Test from outside your network.** Many routers don't send a connection to their own public
  address back inside (no hairpin NAT), so connecting to the public address from the same network
  can fail while it works from anywhere else. Try from a phone's hotspot, and use the local
  address at home.
- **Internet connections fail while local ones work, and the router is fine:** knocks are tied to
  the clock, so the two Macs' clocks must agree within about 10 minutes. Turn on *Set time and
  date automatically* on both.
- **Wi-Fi** adds jitter. For the lowest latency, put the viewed Macs on Ethernet, or connect the two
  Macs with a Thunderbolt cable (Thunderbolt Bridge). On Wi-Fi, a Mac whose radio regularly leaves
  the channel for AirDrop, Universal Control or Sidecar (AWDL) stalls every packet for tens of
  milliseconds a couple of times a second: `ping -i 0.01 <other Mac>` shows it as a run of
  replies 70 ms late. `sudo ifconfig awdl0 down` on that Mac turns AWDL off until it restarts.
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
| `crates/transport` | QUIC endpoint, LAN congestion control, identity, pairing, packetizer/reassembler, the gate that keeps the port silent to the internet (`gate.rs`, `knock.rs`, `cid.rs`), the LanKVM server's protocol and relay on the socket (`rendezvous.rs`, with an in-process server for tests in `test_server.rs`) |
| `crates/platform-mac` | ScreenCaptureKit capture, finding the tiles that changed (`tiler.rs`), VideoToolbox encode/decode, zero-copy GPU import, input injection (`inject.rs`, `keys.rs`), cursor shapes (`cursor.rs`), the clipboard (`clipboard.rs`), microphone capture and playback (`audio.rs`), virtual displays (`virtual_display.rs`), router port mapping (`portmap.rs`) |
| `crates/core` | Host service and its tiled encode pipeline (`host.rs`), viewer sessions (`client.rs`), remote control (`control.rs`), the shared clipboard (`clipboard.rs`), the shared microphone (`microphone.rs`), virtual displays for viewers (`displays.rs`), the names, local addresses and connection types of the Macs this one controls (`address_book.rs`), internet access keys, addresses and port mapping (`internet.rs`), registration with and introductions through the LanKVM server (`rendezvous.rs`), Metal render thread (`view.rs`, `render.rs`), C ABI (`ffi.rs`) for the app |
| `macos/` | SwiftUI app (SwiftPM). `Sources/CLanKVM/include/lankvm.h` is the C interface |
| `macos/AudioDriver` | LanKVM Microphone, the Audio Server plug-in (C) that gives a Mac the microphone other Macs share, and its test |
| `scripts/bundle.sh` | Builds and signs `LanKVM.app` |
| `scripts/build-audio-driver.sh`, `scripts/install-audio-driver.sh` | Build and check LanKVM Microphone; install or remove it from Terminal |
| `scripts/make-icon.swift` | Regenerates `macos/Resources/AppIcon.icns` |
| `scripts/e2e-control.sh`, `scripts/e2e/`, `scripts/input-lab.swift` | One-Mac remote-control tests |
| `scripts/viewer-bench.sh`, `scripts/viewer-report.py`, `macos/Sources/LanKVM/BenchScope.swift` | Source-to-glass bench of the viewer window |

The remote screen never goes through SwiftUI. The viewer window hosts a `CAMetalLayer`, and the
Rust core renders each decoded frame into it from its own thread as soon as it arrives.

Tests: `cargo test --workspace`. This includes a real hardware HEVC encode→decode round trip, a
QUIC loopback session, remote control, the shared clipboard and the shared microphone end to end over QUIC, and
connecting over the (loopback) internet. To review the UI without granting any permissions, render every screen
to PNG (light and dark):

```bash
macos/.build/release/LanKVM --snapshot /tmp/lankvm-shots
```
