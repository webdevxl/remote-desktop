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
        .commands {
            ControlCommands()
        }

        // Shown while another Mac views or controls this one, so whoever sits here can see it
        // and take control back.
        MenuBarExtra(isInserted: Binding(get: { !core.host.viewers.isEmpty }, set: { _ in })) {
            HostMenu().environmentObject(core)
        } label: {
            Image(systemName: core.host.controller != nil ? "cursorarrow.rays" : "eye")
        }
    }
}

/// The Control menu: what the session control offers, for the viewer window in front, plus the
/// settings. (While input goes to the remote Mac every key does too, so these are for the mouse,
/// and for when the session control is hidden.)
private struct ControlCommands: Commands {
    @FocusedObject private var session: SessionModel?
    @FocusedObject private var controls: SessionControlModel?
    @AppStorage(SystemShortcuts.defaultsKey) private var sendSystemShortcuts = true
    @AppStorage(TrackpadGestures.defaultsKey) private var sendTrackpadGestures = true
    @AppStorage(SessionControlModel.visibleKey) private var showSessionControl = true

    var body: some Commands {
        CommandMenu("Control") {
            // A chord of modifiers alone can't be a key equivalent: it's in the titles instead.
            if controls?.isReleased == true {
                Button("Resume Control (⌃⌥⌘)") { controls?.resume() }
            } else {
                Button("Release Keyboard and Mouse (⌃⌥⌘)") { controls?.release() }
                    .disabled(controls?.isForwarding != true)
            }
            if session?.mode == .control {
                Button("View Only") { controls?.viewOnly() }
            } else {
                Button("Control") { controls?.requestControl() }
                    .disabled(!connected)
            }
            Menu("Remote Mac") {
                ForEach(RemoteAction.allCases) { action in
                    Button(action.title) { controls?.perform(action) }
                }
            }
            .disabled(session?.isControlling != true)
            Divider()
            Toggle("Send System Shortcuts to Remote Mac", isOn: $sendSystemShortcuts)
            Text("⌘Tab, ⌘Space, Mission Control and screenshot keys go to the Mac you control.")
            Toggle("Send Trackpad Gestures to Remote Mac", isOn: $sendTrackpadGestures)
            Text("Pinch, rotate and swipes go to the Mac you control.")
            Divider()
            Toggle("Show Session Control", isOn: $showSessionControl)
        }
    }

    private var connected: Bool {
        if case .connected = session?.phase { return true }
        return false
    }
}

/// The menu-bar menu on a Mac others are connected to.
private struct HostMenu: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        ForEach(core.host.viewers) { viewer in
            Text("\(viewer.name) is \(viewer.controlling ? "controlling" : "viewing") this Mac")
        }
        Divider()
        if core.host.controller != nil {
            Button("Stop Control (⌃⌥⌘.)") { core.stopAllControl() }
        }
        Button("Disconnect All") { core.host.viewers.forEach(core.kick) }
        Divider()
        Button("Open LanKVM") {
            NSApp.activate(ignoringOtherApps: true)
            NSApp.windows.first { $0.identifier?.rawValue.hasPrefix("main") == true }?.makeKeyAndOrderFront(nil)
        }
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

    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated {
            // ⌘Tab and Spotlight belong to this Mac again, and no key or button may stay down on
            // either Mac.
            SystemShortcuts.restore()
            CoreModel.shared.shutdown()
        }
    }
}
