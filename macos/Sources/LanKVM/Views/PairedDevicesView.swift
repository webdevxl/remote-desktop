import SwiftUI

struct PairedDevicesView: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        Page {
            PageHeader(
                title: "Paired Devices",
                subtitle: "Devices that entered this Mac's code, and Macs whose code you entered. Forget a device to require the code again."
            )

            if core.paired.viewers.isEmpty && core.paired.hosts.isEmpty {
                Card {
                    ContentUnavailableView {
                        Label("No paired devices", systemImage: "lock.shield")
                    } description: {
                        Text("When you connect to another Mac for the first time, it shows a 6-digit code to type here.")
                    }
                    .padding(.vertical, 24)
                }
            }

            DeviceSection(
                title: "Can view and control this Mac",
                icon: "person.badge.key",
                devices: core.paired.viewers,
                canControlThisMac: true
            )
            DeviceSection(
                title: "Macs you can control",
                icon: "desktopcomputer",
                devices: core.paired.hosts,
                canControlThisMac: false
            )
        }
    }
}

private struct DeviceSection: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.openWindow) private var openWindow
    let title: String
    let icon: String
    let devices: [PairedDevice]
    let canControlThisMac: Bool

    var body: some View {
        if !devices.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                SectionLabel(title: title)
                Card {
                    ForEach(Array(devices.enumerated()), id: \.element.id) { index, device in
                        if index > 0 { CardDivider() }
                        CardRow(icon: icon, tint: .lkAccent, title: device.name, detail: detail(device), monospacedDetail: true) {
                            HStack(spacing: 8) {
                                // A Mac this one controls that it can reach from anywhere: through
                                // the LanKVM server, or at an address it told this Mac.
                                if !canControlThisMac, device.reachable || device.internetAddress != nil {
                                    Button("Connect") { open(device) }
                                        .buttonStyle(SecondaryButtonStyle())
                                        .help(connectHelp(device))
                                }
                                Button("Forget") { core.forget(device, canControlThisMac: canControlThisMac) }
                                    .buttonStyle(SecondaryButtonStyle(destructive: true))
                            }
                        }
                    }
                }
            }
        }
    }

    private func detail(_ device: PairedDevice) -> String {
        guard !canControlThisMac, let address = device.internetAddress else { return "Device ID \(device.deviceId)" }
        // On its own line: joined with " · ", the default window width wraps it after the "·".
        return "Device ID \(device.deviceId)\nInternet \(address)"
    }

    private func connectHelp(_ device: PairedDevice) -> String {
        guard !device.reachable, let address = device.internetAddress else { return "Connect to “\(device.name)” over the internet" }
        return "Connect to “\(device.name)” over the internet at \(address)"
    }

    /// The core tries every way it knows to that Mac ("lankvm:" and its fingerprint); a core that
    /// doesn't say `reachable` only knows the address.
    private func open(_ device: PairedDevice) {
        guard let target = device.reachable ? "lankvm:\(device.fingerprint)" : device.internetAddress else { return }
        let id = core.connect(to: target, label: device.name)
        openWindow(id: "viewer", value: id)
    }
}
