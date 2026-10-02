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
            let info = SessionInfo(hostName: "Studio", address: "192.168.1.31:47800", width: 3024, height: 1964, fps: 60, codec: "Hevc")
            let sample = SessionStats(fps: 60, mbps: 31.4, totalMs: 17.8, captureMs: 4.3, encodeMs: 4.1, networkMs: 0.9, decodeMs: 2.3,
                                      displayMs: 1.9, rttMs: 0.6, framesShown: 1200, framesLost: 0, keyframeRequests: 1)
            render(
                ZStack(alignment: .topLeading) {
                    LinearGradient(colors: [Color(hex: 0x2B4A6F), Color(hex: 0x8A5A44)], startPoint: .topLeading, endPoint: .bottomTrailing)
                    StatsHUD(sessionId: 0, info: info, sample: sample).padding(12)
                }.environmentObject(core),
                size: CGSize(width: 520, height: 320), appearance: appearance,
                to: dir.appendingPathComponent("hud-\(suffix).png"))
            render(EndedView(target: "192.168.1.31", error: "That Mac hasn't allowed Screen Recording for LanKVM yet.", reconnect: {}, close: {}),
                   size: CGSize(width: 640, height: 420), appearance: appearance,
                   to: dir.appendingPathComponent("ended-\(suffix).png"))
        }
        NSApp.terminate(nil)
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
