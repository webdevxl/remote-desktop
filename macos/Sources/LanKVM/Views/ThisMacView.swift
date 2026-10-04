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
                                detail: "\(viewer.controlling ? "Controlling" : "Viewing") · \(viewer.address)\(viewer.internet ? " · over the internet" : "")",
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
        return "Paired Macs can switch to Control and use this Mac's mouse and keyboard. To take it back at any time, press ⌃⌥⌘ and the period key here."
    }
}

/// Whether paired Macs can reach this Mac over the internet, and what to change on the router
/// when they can't.
private struct InternetAccessCard: View {
    @EnvironmentObject private var core: CoreModel

    private var internet: InternetStatus { core.host.internet }

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
                    PublicAddressRow(saved: internet.publicAddress)
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

    /// While the router is asked, or what's wrong and how to fix it.
    @ViewBuilder private var progress: some View {
        if internet.isManual {
            Text(manualText)
                .font(.system(size: 12))
                .foregroundStyle(Color.lkSecondary)
                .fixedSize(horizontal: false, vertical: true)
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
        } else if internet.isOpen {
            StatusPill(text: "Open", color: .lkSuccess)
        } else if internet.isManual {
            StatusPill(text: "Manual setup", color: .lkAccent)
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
        if internet.isOpen { return .lkSuccess }
        if internet.isManual { return .lkAccent }
        return internet.enabled && internet.state == .problem ? .lkWarning : .lkAccent
    }

    /// Only ever promises what's true now; the "no answer" is about the internet (on the network,
    /// a new Mac is asked for a code).
    private var explanation: String {
        if !internet.enabled {
            return "Paired Macs can only connect on your network. Turn this on to let them connect over the internet too."
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

/// The address paired Macs use to reach this one over the internet, as the user typed it. Saved on
/// Return, when the field loses focus, or when it goes away.
private struct PublicAddressRow: View {
    @EnvironmentObject private var core: CoreModel
    /// What the core has now.
    let saved: String
    @State private var draft: String
    @FocusState private var focused: Bool

    init(saved: String) {
        self.saved = saved
        _draft = State(initialValue: saved)
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            IconBadge(systemName: "link")
            VStack(alignment: .leading, spacing: 6) {
                Text("Public address")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Color.lkText)
                HStack(spacing: 8) {
                    TextField("home.example.com or 203.0.113.7", text: $draft)
                        .textFieldStyle(.plain)
                        .font(.system(size: 12, design: .monospaced))
                        .focused($focused)
                        .onSubmit(save)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 5)
                        .background(Color.lkText.opacity(0.04), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .strokeBorder(focused ? Color.lkAccent.opacity(0.7) : Color.lkBorder))
                        .frame(maxWidth: 340)
                    if edited {
                        Button("Save", action: save)
                            .buttonStyle(SecondaryButtonStyle())
                    }
                }
                Text("If your router's address changes, use a dynamic DNS name here. Paired Macs learn it the next time they connect.")
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

    private var trimmed: String { draft.trimmingCharacters(in: .whitespacesAndNewlines) }

    private var edited: Bool { trimmed != saved }

    private func save() {
        guard edited else { return }
        core.setPublicAddress(trimmed)
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
