import AppKit
import ApplicationServices
import SwiftUI

struct ThisMacView: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        Page {
            PageHeader(
                title: "This Mac",
                subtitle: "Other Macs on your network can view and control this one after pairing with it."
            )

            ScreenSharingCard()

            RemoteControlCard()

            MicrophoneCard()

            InternetAccessCard()

            if let mac = core.thisMac {
                VStack(alignment: .leading, spacing: 8) {
                    SectionLabel(title: "Address")
                    Card {
                        CardRow(icon: "laptopcomputer", tint: .lkAccent, title: mac.name, detail: "Device ID \(mac.deviceId)", monospacedDetail: true)
                        ForEach(mac.addresses, id: \.self) { address in
                            CardDivider()
                            CardRow(icon: "network", title: address, detail: "UDP port \(mac.port)") {
                                CopyButton(text: address)
                            }
                        }
                        if mac.addresses.isEmpty {
                            CardDivider()
                            CardRow(icon: "wifi.exclamationmark", tint: .lkWarning, title: "Not connected to a local network")
                        }
                    }
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                SectionLabel(title: "Connected to this Mac now")
                Card {
                    if core.host.viewers.isEmpty {
                        CardRow(icon: "eye.slash", title: "Nobody is connected")
                    }
                    ForEach(Array(core.host.viewers.enumerated()), id: \.element.id) { index, viewer in
                        if index > 0 { CardDivider() }
                        CardRow(icon: viewer.controlling ? "cursorarrow.rays" : "eye", tint: .lkAccent, title: viewer.name,
                                detail: "\(viewer.controlling ? "Controlling" : "Viewing")\(viewer.microphone ? " · Microphone on" : "") · \(route(viewer))",
                                monospacedDetail: true) {
                            HStack(spacing: 8) {
                                if viewer.controlling {
                                    Button("Stop Control") { core.stopControl(viewer) }
                                        .buttonStyle(SecondaryButtonStyle())
                                        .help("Take back the mouse and keyboard; \(viewer.name) keeps viewing (⌃⌥⌘.)")
                                }
                                Button("Disconnect") { core.kick(viewer) }
                                    .buttonStyle(SecondaryButtonStyle(destructive: true))
                            }
                        }
                    }
                }
            }

            if !core.host.virtualDisplays.isEmpty {
                VirtualDisplaysSection()
            }
        }
    }

    /// Where a viewer is, and how it reaches this Mac when it isn't the local network. Through the
    /// LanKVM server, the address is one the core made up for the relay: not worth showing.
    private func route(_ viewer: Viewer) -> String {
        if viewer.relayed { return "over the internet (relayed)" }
        return viewer.internet ? "\(viewer.address) · over the internet" : viewer.address
    }
}

/// Displays this Mac made for the Macs viewing it, each removable: its viewer goes back to this
/// Mac's own screen.
private struct VirtualDisplaysSection: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            SectionLabel(title: "Displays Added by Other Macs")
            Card {
                ForEach(Array(core.host.virtualDisplays.enumerated()), id: \.element.id) { index, display in
                    if index > 0 { CardDivider() }
                    CardRow(icon: "display", tint: display.inUse ? .lkAccent : .lkSecondary, title: "For “\(display.owner)”",
                            detail: detail(display)) {
                        Button("Remove") { core.removeVirtualDisplay(display) }
                            .buttonStyle(SecondaryButtonStyle(destructive: true))
                            .help("Remove this display; “\(display.owner)” goes back to this Mac’s own screen")
                    }
                }
            }
            Text(explanation)
                .font(.system(size: 12))
                .foregroundStyle(Color.lkSecondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 2)
        }
    }

    private func detail(_ display: VirtualDisplay) -> String {
        guard display.inUse else { return "Not in use · removed in about a minute" }
        var text = display.display.sizeText
        if display.hidpi { text += " · looks like \(display.display.looksLikeText)" }
        switch display.arrangement {
        case .only: text += " · this screen shows a copy"
        case .main: text += " · main display"
        case .extend: text += " · next to this screen"
        }
        return text
    }

    /// Where this Mac's windows are, and that it's temporary.
    private var explanation: String {
        let displays = core.host.virtualDisplays
        let owners = Array(Set(displays.map(\.owner)))
        guard owners.count == 1, let owner = owners.first else {
            return "Other Macs added displays to this Mac. Each goes away when its Mac disconnects."
        }
        if displays.allSatisfy({ $0.arrangement == .extend }) {
            return "“\(owner)” added a display next to this Mac’s screen. It goes away when “\(owner)” disconnects."
        }
        return "“\(owner)” added a display to this Mac. Your windows are on it; it goes away when “\(owner)” disconnects."
    }
}

/// Whether others can view this Mac, and how to fix it when they can't.
private struct ScreenSharingCard: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        Card {
            HStack(alignment: .top, spacing: 12) {
                IconBadge(systemName: icon, tint: tint)
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text("Screen sharing")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Color.lkText)
                        Spacer()
                        status
                    }
                    Text(explanation)
                        .font(.system(size: 12))
                        .foregroundStyle(Color.lkSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if core.canShareScreen == false {
                        VStack(alignment: .leading, spacing: 6) {
                            Step(number: 1, text: "Open Privacy Settings. macOS doesn't list LanKVM there by itself.")
                            Step(number: 2, text: "Drag LanKVM from the Finder window into the list, or click + and choose it.")
                            Step(number: 3, text: "Make sure its switch is on, then relaunch LanKVM.")
                        }
                        .padding(.top, 2)
                        // One row when there's room, two when the window is narrow.
                        ViewThatFits(in: .horizontal) {
                            HStack(spacing: 8) { setupButtons; followUpButtons }
                            VStack(alignment: .leading, spacing: 8) {
                                HStack(spacing: 8) { setupButtons }
                                HStack(spacing: 8) { followUpButtons }
                            }
                        }
                        .padding(.top, 4)
                    }
                }
            }
            .padding(14)
        }
    }

    @ViewBuilder private var setupButtons: some View {
        Button("Open Privacy Settings") {
            core.openScreenRecordingSettings()
            core.revealAppInFinder()
        }
        .buttonStyle(PrimaryButtonStyle())
        Button("Show LanKVM in Finder") { core.revealAppInFinder() }
            .buttonStyle(SecondaryButtonStyle())
    }

    @ViewBuilder private var followUpButtons: some View {
        Button("Relaunch") { core.relaunch() }
            .buttonStyle(SecondaryButtonStyle())
        Button("Check Again") { core.verifyScreenCapture() }
            .buttonStyle(SecondaryButtonStyle())
    }

    @ViewBuilder private var status: some View {
        switch core.canShareScreen {
        case .some(true): StatusPill(text: "Ready", color: .lkSuccess)
        case .some(false): StatusPill(text: "Needs permission", color: .lkWarning)
        case .none: ProgressView().controlSize(.small)
        }
    }

    private var icon: String {
        core.canShareScreen == false ? "exclamationmark.triangle" : "rectangle.on.rectangle"
    }

    private var tint: Color {
        core.canShareScreen == false ? .lkWarning : .lkSuccess
    }

    private var explanation: String {
        switch core.canShareScreen {
        case .some(true): "Paired Macs can see this screen. You'll be asked for a code the first time a new Mac connects."
        case .some(false): "macOS hasn't allowed LanKVM to record the screen yet, so other Macs can't view this one."
        case .none: "Checking Screen Recording permission…"
        }
    }
}

/// Whether paired Macs can use this Mac's mouse and keyboard, and how to allow it.
private struct RemoteControlCard: View {
    @EnvironmentObject private var core: CoreModel
    private let poll = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    private var allowed: Binding<Bool> {
        Binding(get: { core.host.allowControl }, set: { core.setAllowControl($0) })
    }

    var body: some View {
        Card {
            HStack(alignment: .top, spacing: 12) {
                IconBadge(systemName: needsPermission ? "exclamationmark.triangle" : "cursorarrow.rays", tint: needsPermission ? .lkWarning : .lkAccent)
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text("Remote control")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Color.lkText)
                        Spacer()
                        status
                    }
                    Text(explanation)
                        .font(.system(size: 12))
                        .foregroundStyle(Color.lkSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Toggle("Let paired Macs control this Mac", isOn: allowed)
                        .toggleStyle(.switch)
                        .controlSize(.small)
                        .font(.system(size: 12))
                        .padding(.top, 2)
                    if needsPermission {
                        VStack(alignment: .leading, spacing: 6) {
                            Step(number: 1, text: "Click Allow Control and choose Open System Settings.")
                            Step(number: 2, text: "Turn on LanKVM under Accessibility.")
                            Step(number: 3, text: "Already on but still not ready? Remove LanKVM with −, add it again, or relaunch LanKVM.")
                        }
                        .padding(.top, 2)
                        HStack(spacing: 8) {
                            Button("Allow Control…") { core.requestControlPermission() }
                                .buttonStyle(PrimaryButtonStyle())
                            Button("Open Accessibility Settings") { core.openAccessibilitySettings() }
                                .buttonStyle(SecondaryButtonStyle())
                            Button("Relaunch") { core.relaunch() }
                                .buttonStyle(SecondaryButtonStyle())
                        }
                        .padding(.top, 4)
                    }
                }
            }
            .padding(14)
        }
        .onReceive(poll) { _ in
            // macOS reports a new Accessibility grant while LanKVM runs; pick it up quickly. Only
            // the cheap check runs every second (the full status asks the permission database,
            // which takes milliseconds on the thread that forwards input).
            if needsPermission && AXIsProcessTrusted() { core.refreshHost() }
        }
    }

    private var needsPermission: Bool { core.host.allowControl && !core.host.controlPermission }

    @ViewBuilder private var status: some View {
        if let controller = core.host.controller {
            StatusPill(text: "Controlled by \(controller.name)", color: .lkAccent)
        } else if !core.host.allowControl {
            StatusPill(text: "Off", color: .lkSecondary)
        } else if needsPermission {
            StatusPill(text: "Needs permission", color: .lkWarning)
        } else {
            StatusPill(text: "Ready", color: .lkSuccess)
        }
    }

    private var explanation: String {
        if !core.host.allowControl {
            return "Paired Macs can only view this screen."
        }
        if needsPermission {
            return "To use this Mac's mouse and keyboard from another Mac, macOS needs to allow LanKVM under Accessibility."
        }
        return "Paired Macs can switch to Control and use this Mac's mouse and keyboard, and its clipboard if they share theirs. To take it back at any time, press ⌃⌥⌘ and the period key here."
    }
}

/// LanKVM Microphone: an input apps here can pick, which plays the microphone of a Mac viewing this
/// one when that Mac shares it. Needs a driver, installed with an administrator password.
private struct MicrophoneCard: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        Card {
            HStack(alignment: .top, spacing: 12) {
                IconBadge(systemName: core.host.microphoneReady ? "mic" : "mic.slash", tint: core.host.microphoneReady ? .lkAccent : .lkSecondary)
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        Text("Microphone")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Color.lkText)
                        Spacer()
                        status
                    }
                    Text(explanation)
                        .font(.system(size: 12))
                        .foregroundStyle(Color.lkSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let error = core.microphoneDriverError {
                        Text(error)
                            .font(.system(size: 12))
                            .foregroundStyle(Color.lkWarning)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    HStack(spacing: 8) {
                        actions
                        if core.microphoneDriverBusy {
                            ProgressView().controlSize(.small)
                        }
                    }
                    .disabled(core.microphoneDriverBusy)
                    .padding(.top, 4)
                }
            }
            .padding(14)
        }
    }

    private var listeners: [Viewer] { core.host.viewers.filter(\.microphone) }

    @ViewBuilder private var status: some View {
        if let viewer = listeners.first {
            StatusPill(text: listeners.count > 1 ? "\(listeners.count) Macs’ microphones on" : "\(viewer.name)’s microphone on", color: .lkAccent)
        } else if core.host.microphoneReady {
            StatusPill(text: core.microphoneDriver.outdated ? "Update available" : "Ready", color: core.microphoneDriver.outdated ? .lkWarning : .lkSuccess)
        } else if core.microphoneDriver.installed {
            StatusPill(text: "Not loaded", color: .lkWarning)
        } else {
            StatusPill(text: "Not installed", color: .lkSecondary)
        }
    }

    @ViewBuilder private var actions: some View {
        if !core.microphoneDriver.installed {
            Button("Install…") { core.installMicrophoneDriver() }
                .buttonStyle(PrimaryButtonStyle())
                .disabled(!core.microphoneDriver.bundled)
                .help("Asks for an administrator password, and restarts this Mac’s sound for a moment")
        } else {
            if core.microphoneDriver.outdated {
                Button("Update…") { core.installMicrophoneDriver() }
                    .buttonStyle(PrimaryButtonStyle())
            }
            if !core.host.microphoneReady {
                Button("Restart Audio…") { core.restartAudio() }
                    .buttonStyle(SecondaryButtonStyle())
                    .help("Restarts this Mac’s sound, which loads LanKVM Microphone")
            }
            Button("Remove…") { core.removeMicrophoneDriver() }
                .buttonStyle(SecondaryButtonStyle(destructive: true))
        }
    }

    private var explanation: String {
        if core.microphoneDriverBusy {
            return "Waiting for this Mac’s sound to restart…"
        }
        if !core.microphoneDriver.installed {
            if !core.microphoneDriver.bundled {
                return "This build of LanKVM doesn’t include LanKVM Microphone. Build the app with scripts/bundle.sh."
            }
            return "Install LanKVM Microphone so a paired Mac that views this one can lend it its microphone, for calls and recordings here. Installing asks for an administrator password and restarts this Mac’s sound for a moment."
        }
        if !core.host.microphoneReady {
            return "LanKVM Microphone is installed, but this Mac’s sound hasn’t loaded it yet. Restart Audio, or restart the Mac."
        }
        let pick = "Apps here can choose LanKVM Microphone as their microphone. It plays the microphone of a paired Mac viewing this one, once that Mac turns on Share Microphone."
        return core.host.allowControl ? pick : pick + " Turn on Let paired Macs control this Mac first: until then they can only view."
    }
}

/// Whether paired Macs can reach this Mac over the internet: through the LanKVM server, with no
/// router setup, or directly once the router forwards the port; and what to change on the router
/// when neither works.
private struct InternetAccessCard: View {
    @EnvironmentObject private var core: CoreModel

    private var internet: InternetStatus { core.host.internet }
    private var server: InternetServer { core.host.internet.server }

    private var enabled: Binding<Bool> {
        Binding(get: { core.host.internet.enabled }, set: { core.setInternetAccess($0) })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Card {
                HStack(alignment: .top, spacing: 12) {
                    IconBadge(systemName: "globe", tint: tint)
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Internet access")
                                .font(.system(size: 13, weight: .semibold))
                                .foregroundStyle(Color.lkText)
                            Spacer()
                            status
                        }
                        Text(explanation)
                            .font(.system(size: 12))
                            .foregroundStyle(Color.lkSecondary)
                            .fixedSize(horizontal: false, vertical: true)
                        Toggle("Let paired Macs connect over the internet", isOn: enabled)
                            .toggleStyle(.switch)
                            .controlSize(.small)
                            .font(.system(size: 12))
                            .padding(.top, 2)
                        if internet.enabled {
                            serverStatus
                            progress
                        }
                    }
                }
                .padding(14)
                if internet.enabled {
                    if internet.isOpen, let address = internet.externalAddress {
                        CardDivider()
                        CardRow(icon: "network", tint: .lkSuccess, title: address, detail: addressDetail(address)) {
                            CopyButton(text: address)
                        }
                    }
                    CardDivider()
                    AddressFieldRow(field: .publicAddress, saved: internet.publicAddress)
                    CardDivider()
                    AddressFieldRow(field: .server, saved: server.address)
                }
            }
            if internet.enabled && internet.ignored > 0 {
                Text(ignoredText)
                    .font(.system(size: 12))
                    .foregroundStyle(Color.lkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 2)
            }
        }
    }

    /// How the LanKVM server is doing, when one is set: the way in that needs no router setup.
    @ViewBuilder private var serverStatus: some View {
        switch server.state {
        case .registered:
            StatusLine(icon: "checkmark.circle.fill", tint: .lkSuccess,
                       text: "Reachable from anywhere through the LanKVM server — no router setup needed.",
                       detail: server.observed.map { "The server sees this Mac at \($0)." })
                .padding(.top, 2)
        case .connecting:
            StatusLine(icon: "arrow.triangle.2.circlepath", text: "Connecting to the LanKVM server…")
                .padding(.top, 2)
        case .unreachable:
            StatusLine(icon: "exclamationmark.triangle.fill", tint: .lkWarning,
                       text: "Couldn't reach the LanKVM server at \(server.address).", detail: unreachableDetail)
                .padding(.top, 2)
        case .off, .other:
            EmptyView()
        }
    }

    /// Only the router's way in is left: say whether there is one. The router's status below
    /// says what's wrong with it.
    private var unreachableDetail: String {
        if internet.isOpen { return "LanKVM keeps trying. Meanwhile, paired Macs connect through your router." }
        if internet.isManual { return "LanKVM keeps trying. Until it gets through, paired Macs can only connect directly, through your router." }
        return "LanKVM keeps trying. Until it gets through, paired Macs can't connect over the internet."
    }

    /// While the router is asked, or what's wrong and how to fix it. Once the LanKVM server
    /// introduces paired Macs, the router only matters for a direct connection.
    @ViewBuilder private var progress: some View {
        if internet.isManual {
            Text(manualText)
                .font(.system(size: 12))
                .foregroundStyle(Color.lkSecondary)
                .fixedSize(horizontal: false, vertical: true)
        } else if server.state == .registered {
            if !internet.isOpen && internet.state == .problem && internet.canForwardByHand {
                Text(directText)
                    .font(.system(size: 12))
                    .foregroundStyle(Color.lkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        } else if server.state == .connecting {
            // The server may make the router's setup unnecessary in a moment.
            EmptyView()
        } else if internet.state == .problem {
            VStack(alignment: .leading, spacing: 6) {
                Text(problemText)
                    .font(.system(size: 12))
                    .foregroundStyle(Color.lkText)
                    .fixedSize(horizontal: false, vertical: true)
                if internet.canForwardByHand {
                    ForEach(Array(manualSteps.enumerated()), id: \.offset) { index, step in
                        Step(number: index + 1, text: step)
                    }
                }
            }
            .padding(.top, 2)
        } else if !internet.isOpen {
            Text("Asking your router to forward UDP port \(String(internet.port)) to this Mac…")
                .font(.system(size: 12))
                .foregroundStyle(Color.lkSecondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder private var status: some View {
        if !internet.enabled {
            StatusPill(text: "Off", color: .lkSecondary)
        } else if internet.isReachable {
            StatusPill(text: "Open", color: .lkSuccess)
        } else if internet.isManual {
            StatusPill(text: "Manual setup", color: .lkAccent)
        } else if server.state == .connecting {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                StatusPill(text: "Connecting…", color: .lkSecondary)
            }
        } else if internet.isUnreachable {
            StatusPill(text: "Not reachable", color: .lkWarning)
        } else if internet.state == .problem {
            StatusPill(text: "Needs setup", color: .lkWarning)
        } else {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                StatusPill(text: "Opening…", color: .lkSecondary)
            }
        }
    }

    private var tint: Color {
        if internet.isReachable { return .lkSuccess }
        if internet.isManual || server.state == .connecting { return .lkAccent }
        return internet.enabled && internet.state == .problem ? .lkWarning : .lkAccent
    }

    /// Only ever promises what's true now; the "no answer" is about the internet (on the network,
    /// a new Mac is asked for a code).
    private var explanation: String {
        if !internet.enabled {
            return "Paired Macs can only connect on your network. Turn this on to let them connect over the internet too."
        }
        if internet.viaServer {
            return "Macs that paired with this one on your network can connect over the internet; anyone else gets no answer. The LanKVM server only introduces your Macs to each other. When routers block a direct path, it passes their traffic along, encrypted so that it can't read it."
        }
        if internet.isOpen {
            return "Macs that paired with this one on your network can connect over the internet. Anyone else on the internet gets no answer."
        }
        return "Once the internet can reach this Mac, Macs that paired with it on your network can connect from anywhere. Anyone else on the internet gets no answer."
    }

    /// The router may give another port than LanKVM's when that one is taken there.
    private func addressDetail(_ address: String) -> String {
        let port = String(internet.port)
        guard internet.state == .mapped else { return "Use this address on your other Macs · UDP port \(port)" }
        let opened = address.split(separator: ":").last.map(String.init) ?? port
        return opened == port ? "Your router opened port \(port)" : "Your router opened port \(opened) (\(port) was taken)"
    }

    private var problemText: String {
        let port = String(internet.port)
        switch internet.problem ?? .other {
        case .noResponse:
            return "Your router didn't answer when LanKVM asked it to open a port. Turn on UPnP or NAT-PMP in its settings, or forward the port yourself."
        case .unsupported:
            return "Your router can't open ports automatically. Forward the port yourself."
        case .disabled:
            return "Automatic port forwarding is turned off on your router. Turn on UPnP or NAT-PMP in its settings, or forward the port yourself."
        case .doubleNat:
            let router = internet.routerAddress.map { " (its address, \($0), is private)" } ?? ""
            return "Your router is behind another router\(router), so LanKVM can't open the port to the internet by itself. Forward UDP port \(port) on the outer router too, or put one of them in bridge mode."
        case .cgnat:
            let router = internet.routerAddress.map { " (your router got \($0))" } ?? ""
            return "Your internet provider shares one public address among many customers\(router), so the internet can't reach this Mac. Ask your provider for a public IPv4 address."
        case .noRouter:
            return "This Mac isn't connected to a network with a router, so the internet can't reach it."
        case .serviceDown:
            return "macOS's network service isn't responding, so LanKVM can't ask your router to open a port. It keeps trying."
        case .firewall:
            return "The macOS Firewall is blocking LanKVM. Allow incoming connections for LanKVM in System Settings → Network → Firewall → Options."
        case .other:
            return "LanKVM couldn't get your router to open a port. Forward the port yourself."
        }
    }

    /// LanKVM can't learn the address the internet sees (behind two routers, not even the
    /// router knows it), so the last step is always to enter it.
    private var manualSteps: [String] {
        let port = String(internet.port)
        let local = internet.localAddress ?? "this Mac"
        var steps: [String]
        if internet.problem == .doubleNat {
            steps = [
                "Forward UDP port \(port) to \(internet.routerAddress ?? "your router") in the outer router's settings.",
                "Make sure your own router forwards UDP port \(port) to \(local).",
            ]
        } else {
            steps = ["Forward UDP port \(port) to \(local) in your router's settings."]
        }
        if let address = internet.localAddress {
            steps.append("Reserve \(address) for this Mac in your router, so the forward keeps pointing at it.")
        }
        steps.append("Enter your network's public address or a dynamic DNS name under Public address below.")
        return steps
    }

    /// The LanKVM server works, but the router didn't open the port: forwarding it by hand lets
    /// paired Macs connect directly when the routers won't let a path through by themselves.
    private var directText: String {
        let port = String(internet.port)
        let local = internet.localAddress ?? "this Mac"
        let forward = internet.problem == .doubleNat
            ? "forward UDP port \(port) to \(internet.routerAddress ?? "your router") on the outer router and to \(local) on yours"
            : "forward UDP port \(port) to \(local) in your router's settings"
        return "For a direct connection without going through the server, \(forward), then enter your network's public address below."
    }

    /// The router didn't open the port and the user entered a public address: they forward it
    /// themselves, which LanKVM can't check.
    private var manualText: String {
        let port = String(internet.port)
        let local = internet.localAddress ?? "this Mac"
        let check = internet.problem == .doubleNat
            ? "Using your manual port forwards: make sure UDP port \(port) on the outer router points to \(internet.routerAddress ?? "your router"), and on your router to \(local)."
            : "Using your manual port forward: make sure UDP port \(port) points to \(local)."
        guard internet.localAddress != nil else { return check }
        return check + " Reserve that address for this Mac in your router so it doesn't change."
    }

    /// The core counts packets, not devices: one scanner, or one forgotten Mac retrying, adds several.
    private var ignoredText: String {
        let count = internet.ignored
        return count == 1 ? "Ignored 1 packet from the internet that didn't come from a paired Mac since LanKVM started."
            : "Ignored \(count.formatted()) packets from the internet that didn't come from a paired Mac since LanKVM started."
    }
}

/// An address for internet access, as the user typed it: the one paired Macs use to reach this
/// Mac, or the LanKVM server's. Saved on Return, when the field loses focus, or when it goes away.
private struct AddressFieldRow: View {
    enum Field {
        case publicAddress
        /// Advanced, so kept to two lines: its title beside the field.
        case server
    }

    @EnvironmentObject private var core: CoreModel
    let field: Field
    /// What the core has now.
    let saved: String
    @State private var draft: String
    @FocusState private var focused: Bool

    init(field: Field, saved: String) {
        self.field = field
        self.saved = saved
        _draft = State(initialValue: saved)
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            IconBadge(systemName: field == .server ? "server.rack" : "link")
            VStack(alignment: .leading, spacing: 6) {
                if field == .server {
                    HStack(spacing: 10) {
                        title
                        input
                    }
                } else {
                    title
                    input
                }
                Text(note)
                    .font(.system(size: 12))
                    .foregroundStyle(Color.lkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
        .onChange(of: focused) { _, isFocused in
            if !isFocused { save() }
        }
        // The core's copy wins once saved (it tidies what was typed).
        .onChange(of: saved) { _, value in draft = value }
        .onDisappear(perform: save)
    }

    private var title: some View {
        Text(field == .server ? "LanKVM server" : "Public address")
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(Color.lkText)
    }

    private var input: some View {
        HStack(spacing: 8) {
            TextField(field == .server ? "178.156.129.211:3478" : "home.example.com or 203.0.113.7", text: $draft)
                .textFieldStyle(.plain)
                .font(.system(size: 12, design: .monospaced))
                .focused($focused)
                .onSubmit(save)
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .background(Color.lkText.opacity(0.04), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .strokeBorder(focused ? Color.lkAccent.opacity(0.7) : Color.lkBorder))
                .frame(maxWidth: field == .server ? 240 : 340)
            if edited {
                Button("Save", action: save)
                    .buttonStyle(SecondaryButtonStyle())
            }
        }
    }

    /// Empty, the server field shows the usual server as its placeholder: say that it's off.
    private var note: String {
        switch field {
        case .publicAddress:
            "If your router's address changes, use a dynamic DNS name here. Paired Macs learn it the next time they connect."
        case .server where saved.isEmpty:
            "Off: paired Macs reach this Mac only through your router. Enter a LanKVM server to connect with no router setup."
        case .server:
            "Introduces paired Macs to this one. Change it only if you run your own; clear it to turn the server off."
        }
    }

    private var trimmed: String { draft.trimmingCharacters(in: .whitespacesAndNewlines) }

    private var edited: Bool { trimmed != saved }

    private func save() {
        guard edited else { return }
        switch field {
        case .publicAddress: core.setPublicAddress(trimmed)
        case .server: core.setRendezvousServer(trimmed)
        }
    }
}

/// A line of status in a card: an icon beside a sentence, and a detail under it. The sentence
/// lines up with the text of the steps.
private struct StatusLine: View {
    let icon: String
    var tint = Color.lkSecondary
    let text: String
    var detail: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: icon)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(text)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Color.lkText)
                    .fixedSize(horizontal: false, vertical: true)
                if let detail {
                    Text(detail)
                        .font(.system(size: 12))
                        .foregroundStyle(Color.lkSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                }
            }
        }
    }
}

private struct Step: View {
    let number: Int
    let text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text("\(number)")
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(.white)
                .frame(width: 16, height: 16)
                .background(Color.lkAccent, in: Circle())
            Text(text)
                .font(.system(size: 12))
                .foregroundStyle(Color.lkText)
        }
    }
}

struct CopyButton: View {
    let text: String
    @State private var copied = false

    var body: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            copied = true
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { copied = false }
        } label: {
            Image(systemName: copied ? "checkmark" : "doc.on.doc")
                .frame(width: 14)
        }
        .buttonStyle(SecondaryButtonStyle())
        .help("Copy address")
    }
}
