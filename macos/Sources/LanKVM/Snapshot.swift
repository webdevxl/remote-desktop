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
        }
        NSApp.terminate(nil)
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
