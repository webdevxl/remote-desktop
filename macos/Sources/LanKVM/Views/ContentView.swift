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
            if !Self.autoConnected, let address = ProcessInfo.processInfo.environment["LANKVM_CONNECT"], !address.isEmpty {
                Self.autoConnected = true
                // This window may appear before the app finished launching: the core first.
                core.start()
                openWindow(id: "viewer", value: core.connect(to: address))
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
