import AppKit
import SwiftUI

/// `LanKVM --snapshot <dir>` renders each screen off-screen to PNG (light and dark) and quits.
/// Used to review the UI without screen-recording permissions.
@MainActor
enum Snapshot {
    static func run(into dir: URL) {
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let core = CoreModel.shared
        for (appearance, suffix) in [(NSAppearance.Name.aqua, "light"), (.darkAqua, "dark")] {
            for item in SidebarItem.allCases {
                core.loadSampleState(screenAllowed: item != .thisMac || suffix == "dark")
                render(ContentView(selection: item).environmentObject(core), size: CGSize(width: 900, height: 620),
                       appearance: appearance, to: dir.appendingPathComponent("\(item.rawValue)-\(suffix).png"))
            }
            render(
                PairingRequestSheet(request: PairingRequest(id: 1, name: "Studio", address: "192.168.1.31", pin: "481920"))
                    .environmentObject(core),
                size: CGSize(width: 420, height: 380), appearance: appearance,
                to: dir.appendingPathComponent("pairing-\(suffix).png"))
            render(
                ZStack { Color.lkBackground; PinEntryView(hostLabel: "192.168.1.31", onSubmit: { _ in }, onCancel: {}) },
                size: CGSize(width: 640, height: 460), appearance: appearance,
                to: dir.appendingPathComponent("pin-entry-\(suffix).png"))
            let info = SessionInfo(hostName: "Studio", hostId: "9f12:0ab3:77c1:e402", address: "192.168.1.31:47800", width: 3024, height: 1964, fps: 60, codec: "Hevc")
            let sample = SessionStats(fps: 60, mbps: 31.4, totalMs: 17.8, captureMs: 4.3, encodeMs: 4.1, networkMs: 0.9, decodeMs: 2.3,
                                      displayMs: 1.9, rttMs: 0.6, framesShown: 1200, framesLost: 0, keyframeRequests: 1,
                                      inputMs: 0.4, inputsSent: 5210)
            render(
                ZStack(alignment: .topLeading) {
                    LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                    StatsHUD(sessionId: 0, info: info, sample: sample).padding(12)
                }.environmentObject(core),
                size: CGSize(width: 520, height: 320), appearance: appearance,
                to: dir.appendingPathComponent("hud-\(suffix).png"))
            let refused = SessionModel(id: 0, target: "192.168.1.31")
            refused.control = .refused(ControlReason.needsPermission, "Studio hasn't allowed LanKVM to control it yet. On that Mac, turn on LanKVM in System Settings → Privacy & Security → Accessibility.")
            let inUse = SessionModel(id: 0, target: "192.168.1.31")
            inUse.control = .refused(ControlReason.inUse, "Mac mini is controlling Studio right now.")
            let asking = SessionModel(id: 0, target: "192.168.1.31")
            asking.control = .requesting
            render(
                ZStack(alignment: .top) {
                    LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                    VStack(spacing: 12) {
                        ControlBanner(session: refused, hostName: "Studio")
                        ControlBanner(session: inUse, hostName: "Studio")
                        ControlBanner(session: asking, hostName: "Studio")
                    }
                    .padding(.top, 14)
                },
                size: CGSize(width: 640, height: 260), appearance: appearance,
                to: dir.appendingPathComponent("control-banners-\(suffix).png"))
            render(EndedView(target: "192.168.1.31", error: "That Mac hasn't allowed Screen Recording for LanKVM yet.", reconnect: {}, close: {}),
                   size: CGSize(width: 640, height: 420), appearance: appearance,
                   to: dir.appendingPathComponent("ended-\(suffix).png"))
            renderSessionControls(info: info, stats: sample, appearance: appearance, suffix: suffix, into: dir)
            renderGestureHint(info: info, stats: sample, appearance: appearance, suffix: suffix, into: dir)
            renderDisplays(stats: sample, appearance: appearance, suffix: suffix, into: dir)
            renderInternet(info: info, stats: sample, appearance: appearance, suffix: suffix, into: dir)
        }
        NSApp.terminate(nil)
    }

    /// Internet access: This Mac when the router opened the port, when it needs setting up by hand
    /// (behind one router or two), once it was set up by hand, through the LanKVM server, and when
    /// neither the router nor the server answers; the Macs this one controls with their internet
    /// address or through the server, a recent Mac reached through the server, and the
    /// statistics of a session over the internet, direct and relayed.
    private static func renderInternet(info: SessionInfo, stats: SessionStats, appearance: NSAppearance.Name, suffix: String,
                                       into dir: URL) {
        let core = CoreModel.shared
        for (name, internet) in [("open", CoreModel.SampleInternet.open), ("setup", .setup), ("doublenat", .doubleNat), ("manual", .manual),
                                 ("server", .server), ("noserver", .noServer)] {
            core.loadSampleState(screenAllowed: true, internet: internet)
            // The server's lines make the card longer: still down to the Macs connected now.
            let height: CGFloat = internet == .server || internet == .noServer ? 1250 : 1100
            render(ContentView(selection: .thisMac).environmentObject(core), size: CGSize(width: 900, height: height),
                   appearance: appearance, to: dir.appendingPathComponent("thisMac-internet-\(name)-\(suffix).png"))
        }
        for (name, internet) in [("internet", CoreModel.SampleInternet.open), ("server", .server)] {
            core.loadSampleState(screenAllowed: true, internet: internet)
            render(ContentView(selection: .paired).environmentObject(core), size: CGSize(width: 900, height: 620),
                   appearance: appearance, to: dir.appendingPathComponent("paired-\(name)-\(suffix).png"))
        }
        // A Mac connected to through the server is a recent without an address to show.
        core.loadSampleState(screenAllowed: true, internet: .server)
        render(ContentView(selection: .connect).environmentObject(core), size: CGSize(width: 900, height: 620),
               appearance: appearance, to: dir.appendingPathComponent("connect-server-\(suffix).png"))

        var remote = info
        remote.address = "198.51.100.17:47800"
        remote.internet = true
        var slower = stats
        slower.mbps = 11.2
        slower.networkMs = 19.4
        slower.totalMs = 36.3
        slower.rttMs = 38.2
        var relayed = remote
        relayed.relayed = true
        for (name, info) in [("internet", remote), ("relayed", relayed)] {
            render(
                ZStack(alignment: .topLeading) {
                    LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                    StatsHUD(sessionId: 0, info: info, sample: slower).padding(12)
                }.environmentObject(core),
                size: CGSize(width: 520, height: 320), appearance: appearance,
                to: dir.appendingPathComponent("hud-\(name)-\(suffix).png"))
        }
    }

    /// Virtual displays: the banners while switching and when it went wrong, the dimmed screen
    /// while switching, the Custom Size sheet, connecting with a display to set up, the
    /// statistics at a display's size, and the host's list of displays made for other Macs.
    private static func renderDisplays(stats: SessionStats, appearance: NSAppearance.Name, suffix: String, into dir: URL) {
        let core = CoreModel.shared
        let display = RemoteDisplay(kind: .virtual, width: 6144, height: 2560, hidpi: true, refreshHz: 60, arrangement: .only)
        let info = SessionInfo(hostName: "Studio", hostId: "9f12:0ab3:77c1:e402", address: "192.168.1.31:47800", width: 6144, height: 2560,
                               fps: 60, codec: "Hevc", display: display)
        let switching = sampleSession(info: info)
        switching.display = .switching(request: 2, label: display.sizeText, fromConnect: false)
        let failed = sampleSession(info: info)
        failed.display = .failed(DisplayReason.failed,
                                 "“Studio” couldn't add the display: macOS refused to make it. Try again in a moment.", retry: display)
        let removed = sampleSession(info: info)
        removed.display = .failed(DisplayReason.removedByHost,
                                  "Someone on “Studio” removed its LanKVM display, so this window shows its own screen.", retry: display)
        let offered = sampleSession(info: info)
        offered.display = .offered(display)
        let undecodable = sampleSession(info: info)
        undecodable.streamError = "This Mac can't decode the 6144×2560 picture (the decoder refused the size)."
        render(
            ZStack(alignment: .top) {
                LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                VStack(spacing: 12) {
                    ForEach(Array([switching, failed, removed, offered, undecodable].enumerated()), id: \.offset) { _, session in
                        DisplayBanner(session: session, controls: session.sessionControl, info: info)
                    }
                }
                .padding(.top, 14)
            },
            size: CGSize(width: 640, height: 330), appearance: appearance,
            to: dir.appendingPathComponent("display-banners-\(suffix).png"))

        // The whole screen while switching: dimmed, with the banner, the statistics and the
        // session control.
        let session = sampleSession(info: info)
        session.control = .active
        session.display = .switching(request: 2, label: display.sizeText, fromConnect: false)
        session.sessionControl.showSample(forwarding: true)
        render(
            ZStack {
                LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                ScreenOverlays(session: session, controls: session.sessionControl, info: info, sampleStats: stats)
            }
            .padding(.top, 28)
            .environmentObject(core),
            size: CGSize(width: 1000, height: 480), appearance: appearance,
            to: dir.appendingPathComponent("display-switching-\(suffix).png"))

        for (name, draft) in [("custom-display", DisplayDraft(width: 6144, height: 2560, hidpi: true, refreshHz: 60)),
                              ("custom-display-invalid", DisplayDraft(width: 9001, height: 2560, hidpi: true, refreshHz: 120))] {
            render(CustomDisplaySheet(draft: draft, hostName: "Studio", apply: { _ in }, cancel: {}),
                   size: CGSize(width: 460, height: 330), appearance: appearance,
                   to: dir.appendingPathComponent("\(name)-\(suffix).png"))
        }
        render(ConnectingView(target: "Studio", detail: "Setting up a \(display.sizeText) display…", cancel: {}),
               size: CGSize(width: 640, height: 420), appearance: appearance,
               to: dir.appendingPathComponent("connecting-display-\(suffix).png"))
        render(
            ZStack(alignment: .topLeading) {
                LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                StatsHUD(sessionId: 0, info: info, sample: stats).padding(12)
            }.environmentObject(core),
            size: CGSize(width: 520, height: 320), appearance: appearance,
            to: dir.appendingPathComponent("hud-display-\(suffix).png"))

        core.loadSampleState(screenAllowed: true, displays: true)
        render(ContentView(selection: .thisMac).environmentObject(core), size: CGSize(width: 900, height: 1100),
               appearance: appearance, to: dir.appendingPathComponent("thisMac-displays-\(suffix).png"))
    }

    /// While controlling without Accessibility here: the hint that Dock gestures still act on this
    /// Mac, under the session control.
    private static func renderGestureHint(info: SessionInfo, stats: SessionStats, appearance: NSAppearance.Name,
                                          suffix: String, into dir: URL) {
        let session = sampleSession(info: info)
        session.control = .active
        session.sessionControl.showSample(forwarding: true)
        render(
            ZStack {
                LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                ScreenOverlays(session: session, controls: session.sessionControl, info: info, sampleStats: stats, sampleTrusted: false)
            }
            .padding(.top, 28)
            .environmentObject(CoreModel.shared),
            size: CGSize(width: 1000, height: 400), appearance: appearance,
            to: dir.appendingPathComponent("gesture-hint-\(suffix).png"))
    }

    /// The session control in each state and on each kind of edge, then over a whole screen
    /// with the other overlays keeping clear of it.
    private static func renderSessionControls(info: SessionInfo, stats: SessionStats, appearance: NSAppearance.Name,
                                              suffix: String, into dir: URL) {
        let wide = CGSize(width: 560, height: 96)
        let tall = CGSize(width: 560, height: 220)
        let left = Placement(edge: .left, fraction: 0.5)
        let bottom = Placement(edge: .bottom, fraction: 0.5)
        render(
            VStack(alignment: .leading, spacing: 10) {
                Grid(horizontalSpacing: 10, verticalSpacing: 10) {
                    GridRow {
                        controlTile("Controlling, collapsed (idle)", info: info, size: wide) { $0.control = .active; $1.showSample(forwarding: true) }
                        controlTile("Released, collapsed", info: info, size: wide) { $0.control = .active; $1.showSample(released: true) }
                    }
                    GridRow {
                        controlTile("Controlling, expanded", info: info, size: wide) { $0.control = .active; $1.showSample(forwarding: true, expanded: true) }
                        controlTile("Released, expanded", info: info, size: wide) { $0.control = .active; $1.showSample(released: true, expanded: true) }
                    }
                    GridRow {
                        controlTile("Viewing (full screen)", info: info, size: wide) { $0.control = .off; $1.showSample(fullScreen: true, expanded: true) }
                        controlTile("Asking for control", info: info, size: wide) { $0.control = .requesting; $1.showSample(expanded: true) }
                    }
                    GridRow {
                        controlTile("Left edge, collapsed", info: info, size: tall) { $0.control = .active; $1.showSample(forwarding: true, placement: left) }
                        controlTile("Left edge, expanded", info: info, size: tall) { $0.control = .active; $1.showSample(forwarding: true, expanded: true, placement: left) }
                    }
                    GridRow {
                        controlTile("Over a white screen", info: info, size: wide, background: .white) { $0.control = .active; $1.showSample(forwarding: true, expanded: true) }
                        controlTile("Increased contrast", info: info, size: wide) { $0.control = .active; $1.showSample(forwarding: true, expanded: true) }
                            .environment(\._colorSchemeContrast, .increased)
                    }
                    GridRow {
                        controlTile("Narrow window", info: info, size: CGSize(width: 480, height: 96)) { $0.control = .active; $1.showSample(forwarding: true, expanded: true) }
                        controlTile("Paused (window in the background)", info: info, size: wide, captionAt: .topTrailing) {
                            $0.control = .active
                            $1.showSample(expanded: true, placement: bottom)
                        }
                    }
                }
            }
            .padding(EdgeInsets(top: 38, leading: 10, bottom: 10, trailing: 10))
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .background(Color.lkBackground)
            .environmentObject(CoreModel.shared),
            size: CGSize(width: 1150, height: 800), appearance: appearance,
            to: dir.appendingPathComponent("session-control-\(suffix).png"))

        // A whole screen: the statistics, the test-mode banner and a toast stay clear of the
        // control (top-docked, then bottom-docked).
        for (name, placement) in [("top", Placement.default), ("bottom", bottom)] {
            let session = sampleSession(info: info)
            session.control = .active
            session.sameMachine = true
            session.showToast("Controlling Studio · press ⌃⌥⌘ to release")
            session.sessionControl.showSample(forwarding: true, expanded: true, placement: placement)
            render(
                ZStack {
                    LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                    ScreenOverlays(session: session, controls: session.sessionControl, info: info, sampleStats: stats)
                }
                .padding(.top, 28)
                .environmentObject(CoreModel.shared),
                size: CGSize(width: 1000, height: 648), appearance: appearance,
                to: dir.appendingPathComponent("session-overlays-\(name)-\(suffix).png"))
        }
    }

    private static func sampleSession(info: SessionInfo) -> SessionModel {
        let session = SessionModel(id: 0, target: "192.168.1.31")
        session.phase = .connected(info)
        return session
    }

    /// One state of the session control over a stand-in for the remote screen, with a caption.
    private static func controlTile(_ caption: String, info: SessionInfo, size: CGSize, background: Color? = nil,
                                    captionAt: Alignment = .bottomTrailing,
                                    setUp: (SessionModel, SessionControlModel) -> Void) -> some View {
        let session = sampleSession(info: info)
        setUp(session, session.sessionControl)
        return ZStack(alignment: captionAt) {
            if let background {
                background
            } else {
                LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
            }
            SessionControl(session: session, model: session.sessionControl)
            Text(caption)
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(background == nil ? .white.opacity(0.6) : .black.opacity(0.45))
                .padding(8)
        }
        .frame(width: size.width, height: size.height)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    private static func render<V: View>(_ view: V, size: CGSize, appearance: NSAppearance.Name, to url: URL) {
        let window = NSWindow(
            contentRect: CGRect(origin: CGPoint(x: -10_000, y: -10_000), size: size),
            styleMask: [.titled, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.appearance = NSAppearance(named: appearance)
        window.titlebarAppearsTransparent = true
        let host = NSHostingView(rootView: view.frame(width: size.width, height: size.height))
        window.contentView = host
        window.orderFrontRegardless()
        host.layoutSubtreeIfNeeded()
        RunLoop.current.run(until: Date().addingTimeInterval(0.4))
        guard let rep = host.bitmapImageRepForCachingDisplay(in: host.bounds) else { return }
        host.cacheDisplay(in: host.bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
        window.orderOut(nil)
    }
}
