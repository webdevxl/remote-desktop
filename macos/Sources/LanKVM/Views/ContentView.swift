import Combine
import SwiftUI

enum SidebarItem: String, CaseIterable, Identifiable {
    case connect, thisMac, paired

    var id: String { rawValue }

    var title: String {
        switch self {
        case .connect: "Connect"
        case .thisMac: "This Mac"
        case .paired: "Paired Devices"
        }
    }

    var icon: String {
        switch self {
        case .connect: "display.2"
        case .thisMac: "laptopcomputer"
        case .paired: "lock.shield"
        }
    }
}

struct ContentView: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.openWindow) private var openWindow
    @State private var selection: SidebarItem?
    /// LANKVM_CONNECT is used once per launch.
    @MainActor private static var autoConnected = false
    /// Waits for that session to connect, given LANKVM_CONNECT_DISPLAY.
    @MainActor private static var connectDisplay: AnyCancellable?

    init(selection: SidebarItem = .connect) {
        _selection = State(initialValue: selection)
    }

    var body: some View {
        NavigationSplitView {
            Sidebar(selection: $selection)
                .navigationSplitViewColumnWidth(min: 200, ideal: 220, max: 260)
        } detail: {
            Group {
                if let error = core.startError {
                    StartErrorView(message: error)
                } else {
                    switch selection ?? .connect {
                    case .connect: ConnectView()
                    case .thisMac: ThisMacView()
                    case .paired: PairedDevicesView()
                    }
                }
            }
            .background(Color.lkBackground)
        }
        .tint(.lkAccent)
        .sheet(item: Binding(get: { core.host.pairing.first }, set: { _ in })) { request in
            PairingRequestSheet(request: request)
        }
        .onAppear {
            // Test runs (scripts/e2e-control.sh viewer): connect at once, as if typed in Connect.
            let env = ProcessInfo.processInfo.environment
            if !Self.autoConnected, let address = env["LANKVM_CONNECT"], !address.isEmpty {
                Self.autoConnected = true
                // This window may appear before the app finished launching: the core first.
                core.start()
                let id = core.connect(to: address)
                openWindow(id: "viewer", value: id)
                // And a display of this size next to the host's own (scripts/latency-bench.sh VIEWER=1).
                if let spec = env["LANKVM_CONNECT_DISPLAY"], let display = Self.testDisplay(spec) {
                    Self.showOnceConnected(display, session: id)
                }
            }
        }
    }

    /// LANKVM_CONNECT_DISPLAY's "WxH[@2x][@HZ]" (pixels; @2x: Retina; HZ: its refresh rate, 60
    /// by default) as a display next to the host's own screens; nil if it isn't one.
    static func testDisplay(_ spec: String) -> RemoteDisplay? {
        var parts = spec.lowercased().split(separator: "@").map(String.init)
        let size = parts.isEmpty ? [] : parts.removeFirst().split(separator: "x").compactMap { UInt32($0) }
        guard size.count == 2 else { return nil }
        var display = RemoteDisplay(kind: .virtual, width: size[0], height: size[1], arrangement: .extend)
        for part in parts {
            if part == "2x" {
                display.hidpi = true
            } else if let hz = UInt32(part.hasSuffix("hz") ? String(part.dropLast(2)) : part) {
                display.refreshHz = hz
            } else {
                return nil
            }
        }
        return display
    }

    /// Asks for `display` once the session connects, as if picked on connecting (like the display
    /// remembered for a host: the window shows no picture of the host's own screen meanwhile, and
    /// it isn't remembered).
    @MainActor private static func showOnceConnected(_ display: RemoteDisplay, session id: UInt64) {
        guard let session = CoreModel.shared.session(id) else { return }
        connectDisplay = session.$phase
            .first { phase in
                if case .connected = phase { return true }
                return false
            }
            // $phase tells before the change is made: ask once it is, after what connecting asks for.
            .receive(on: DispatchQueue.main)
            .sink { _ in
                MainActor.assumeIsolated {
                    CoreModel.shared.showDisplay(display, for: id, fromConnect: true)
                    connectDisplay = nil
                }
            }
    }
}

struct Sidebar: View {
    @EnvironmentObject private var core: CoreModel
    @Binding var selection: SidebarItem?

    var body: some View {
        VStack(spacing: 0) {
            List(selection: $selection) {
                Section {
                    ForEach(SidebarItem.allCases) { item in
                        Label(item.title, systemImage: item.icon)
                            .badge(item == .thisMac ? core.host.viewers.count : 0)
                            .tag(item)
                    }
                }
            }
            .listStyle(.sidebar)
            .scrollContentBackground(.hidden)

            SidebarFooter()
        }
        .background(Color.lkSidebar)
    }
}

/// Shows at a glance whether this Mac can be viewed.
struct SidebarFooter: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "antenna.radiowaves.left.and.right")
                .foregroundStyle(core.canShareScreen == true ? Color.lkSuccess : Color.lkWarning)
            VStack(alignment: .leading, spacing: 1) {
                Text(core.thisMac?.name ?? "LanKVM")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Color.lkText)
                    .lineLimit(1)
                Text(statusText)
                    .font(.system(size: 11))
                    .foregroundStyle(Color.lkSecondary)
                    .lineLimit(1)
            }
            Spacer()
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .overlay(alignment: .top) { Rectangle().fill(Color.lkBorder).frame(height: 1) }
    }

    private var statusText: String {
        switch core.canShareScreen {
        case .some(true): core.thisMac?.addresses.first.map { "Reachable at \($0)" } ?? "Reachable"
        case .some(false): "Screen sharing not allowed"
        case .none: "Checking…"
        }
    }
}

struct StartErrorView: View {
    let message: String

    var body: some View {
        ContentUnavailableView {
            Label("LanKVM couldn't start", systemImage: "exclamationmark.triangle")
        } description: {
            Text(message)
        }
    }
}
