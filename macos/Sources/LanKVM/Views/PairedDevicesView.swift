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
                        CardRow(icon: icon, tint: .lkAccent, title: device.name, detail: "Device ID \(device.deviceId)", monospacedDetail: true) {
                            Button("Forget") { core.forget(device, canControlThisMac: canControlThisMac) }
                                .buttonStyle(SecondaryButtonStyle(destructive: true))
                        }
                    }
                }
            }
        }
    }
}
