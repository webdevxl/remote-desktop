import SwiftUI

/// A Mac this one controls under Your Macs (Connect): its name, its two addresses, how to connect
/// to it, and Connect.
struct MacRow: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.openWindow) private var openWindow
    let device: PairedDevice
    @State private var hovering = false
    @State private var renaming = false

    var body: some View {
        HStack(spacing: 12) {
            IconBadge(systemName: "desktopcomputer", tint: .lkAccent)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 4) {
                    Text(device.displayName)
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(Color.lkText)
                        .lineLimit(1)
                    Button { renaming = true } label: {
                        Image(systemName: "pencil")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Color.lkSecondary)
                            .frame(width: 18, height: 18)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("Rename “\(device.displayName)”")
                    .accessibilityLabel("Rename")
                    .opacity(hovering || renaming ? 1 : 0)
                    .renamePopover(device: device, isPresented: $renaming)
                }
                MacAddresses(device: device)
            }
            Spacer(minLength: 8)
            ConnectionPicker(device: device)
            Button("Connect", action: connect)
                .buttonStyle(SecondaryButtonStyle())
                .disabled(!device.canConnect(device.connection))
                .help(connectHelp(device, dht: core.host.internet.dht.enabled))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .contextMenu {
            Button("Rename…") { renaming = true }
            Picker("Connect Over", selection: connection) {
                ForEach(ConnectionType.allCases) { type in
                    Text(type.menuTitle).tag(type)
                }
            }
        }
    }

    private var connection: Binding<ConnectionType> {
        Binding(get: { device.connection }, set: { core.setConnection($0, for: device) })
    }

    private func connect() {
        let id = core.connect(to: device)
        openWindow(id: "viewer", value: id)
    }
}

/// Connect's tooltip: the way it goes, or why there is none. `dht`: this Mac looks for paired
/// Macs on the BitTorrent DHT too.
func connectHelp(_ device: PairedDevice, dht: Bool) -> String {
    let name = "“\(device.displayName)”"
    guard device.canConnect(device.connection) else {
        switch device.connection {
        case .local:
            return "This Mac doesn't know where \(name) is on the local network yet. Type its address in Connect once (This Mac in LanKVM on that Mac shows it)."
        case .internet:
            return "\(name) hasn't told this Mac how to reach it over the internet. Turn on internet access on it (This Mac in LanKVM), then connect to it once on the local network."
        case .auto:
            return "This Mac doesn't know where \(name) is yet. Type its address in Connect once, on the same network (This Mac in LanKVM on that Mac shows it)."
        }
    }
    switch device.connection {
    case .auto:
        return "Connect to \(name) on the local network or over the internet, whichever answers (the local network when both do)"
    case .local:
        return "Connect to \(name) on the local network, at \(device.localAddress.map(shownAddress) ?? "its address there")"
    case .internet:
        guard let address = device.internetAddress else {
            return dht ? "Connect to \(name) over the internet, through the LanKVM server or the BitTorrent DHT"
                : "Connect to \(name) over the internet, through the LanKVM server"
        }
        return "Connect to \(name) over the internet (\(shownAddress(address)))"
    }
}

/// A Mac's two addresses, on the local network and on the internet. The one its connection type
/// leaves out is dimmed.
struct MacAddresses: View {
    @EnvironmentObject private var core: CoreModel
    let device: PairedDevice

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 8, verticalSpacing: 2) {
            line("Local", device.localAddress.map(shownAddress), unknown: "Not known yet", used: device.connection != .internet)
                .help(device.localAddress == nil
                    ? "Type its address in Connect once, on the same network: This Mac in LanKVM on that Mac shows it."
                    : "Where this Mac last reached it on the local network")
            line("Internet", device.internetAddress.map(shownAddress), unknown: device.reachable ? foundThrough : "Not set up",
                 used: device.connection != .local)
                .help(device.reachable
                    ? "Where it was reached over the internet, or said to reach it. \(findsIt)"
                    : "Turn on internet access on that Mac (This Mac in LanKVM), then connect to it once on the local network.")
        }
    }

    /// This Mac looks for paired Macs on the BitTorrent DHT too, when it's on.
    private var dht: Bool { core.host.internet.dht.enabled }

    private var foundThrough: String { dht ? "Through the server or the DHT" : "Through the LanKVM server" }

    private var findsIt: String {
        dht ? "The LanKVM server or the BitTorrent DHT finds it wherever it is." : "The LanKVM server finds it wherever it is."
    }

    private func line(_ label: String, _ address: String?, unknown: String, used: Bool) -> some View {
        GridRow {
            Text(label)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(Color.lkSecondary)
            Text(address ?? unknown)
                .font(address == nil ? .system(size: 12) : .system(size: 12, design: .monospaced))
                .foregroundStyle(address == nil ? Color.lkSecondary : Color.lkText.opacity(0.85))
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
        }
        .opacity(used ? 1 : 0.45)
    }
}

/// Auto, Local or Internet: how connecting to the Mac by name reaches it.
struct ConnectionPicker: View {
    @EnvironmentObject private var core: CoreModel
    let device: PairedDevice

    var body: some View {
        Picker("Connect over", selection: Binding(get: { device.connection }, set: { core.setConnection($0, for: device) })) {
            ForEach(ConnectionType.allCases) { type in
                Text(type.shortTitle).tag(type)
            }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .controlSize(.small)
        .fixedSize()
        .help("How to connect to “\(device.displayName)”. Auto: on the local network and over the internet at once, keeping the local network when both answer. Local: only on the local network. Internet: only over the internet, directly or through the LanKVM server.")
    }
}

extension ConnectionType {
    /// In menus.
    var menuTitle: String {
        switch self {
        case .auto: "Automatically"
        case .local: "Local Network"
        case .internet: "Internet"
        }
    }
}

extension View {
    /// A popover to give `device` a name of the user's own.
    func renamePopover(device: PairedDevice, isPresented: Binding<Bool>) -> some View {
        popover(isPresented: isPresented, arrowEdge: .bottom) {
            RenameMacForm(device: device) { isPresented.wrappedValue = false }
        }
    }
}

/// Its own view for snapshots: popovers don't render off-screen.
struct RenameMacForm: View {
    @EnvironmentObject private var core: CoreModel
    let device: PairedDevice
    let done: () -> Void
    @State private var name: String
    @FocusState private var focused: Bool

    init(device: PairedDevice, done: @escaping () -> Void) {
        self.device = device
        self.done = done
        _name = State(initialValue: device.displayName)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Name")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Color.lkText)
            TextField(device.name, text: $name)
                .textFieldStyle(.roundedBorder)
                .focused($focused)
                .onSubmit(save)
            Text("What LanKVM on this Mac calls it. Leave it empty for “\(device.name)”, the name it gave itself.")
                .font(.system(size: 11))
                .foregroundStyle(Color.lkSecondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Spacer()
                Button("Cancel", action: done)
                    .keyboardShortcut(.cancelAction)
                Button("Rename", action: save)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(14)
        .frame(width: 280)
        .onAppear { focused = true }
    }

    private func save() {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        core.setAlias(trimmed == device.name ? "" : trimmed, for: device)
        done()
    }
}
