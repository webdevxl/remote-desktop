import AppKit
import SwiftUI

@main
struct LanKVMApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var core = CoreModel.shared

    var body: some Scene {
        WindowGroup("LanKVM", id: "main") {
            ContentView()
                .environmentObject(core)
                .frame(minWidth: 760, minHeight: 520)
        }
        .defaultSize(width: 900, height: 620)
        .windowToolbarStyle(.unified(showsTitle: false))
        .commands {
            CommandGroup(replacing: .newItem) {}
        }

        WindowGroup("Remote Mac", id: "viewer", for: UInt64.self) { $sessionId in
            if let sessionId {
                ViewerView(sessionId: sessionId)
                    .environmentObject(core)
            }
        }
        .defaultSize(width: 1280, height: 800)
        .windowToolbarStyle(.unified(showsTitle: true))
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        let args = CommandLine.arguments
        if let i = args.firstIndex(of: "--snapshot"), i + 1 < args.count {
            MainActor.assumeIsolated { Snapshot.run(into: URL(fileURLWithPath: args[i + 1])) }
            return
        }
        MainActor.assumeIsolated { CoreModel.shared.start() }
    }

    /// Keep running (and reachable by other Macs) when the window is closed.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }
}
