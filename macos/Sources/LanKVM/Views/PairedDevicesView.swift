import SwiftUI

struct PairedDevicesView: View {
    @EnvironmentObject private var core: CoreModel

    var body: some View {
        Page {
            PageHeader(
                title: "Paired Devices",
                subtitle: "Devices that entered this Mac's code, and Macs whose code you entered. Rename a Mac you control to call it something else here; forget a device to require the code again."
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
                        if canControlThisMac {
                            CardRow(icon: icon, tint: .lkAccent, title: device.name, detail: "Device ID \(device.deviceId)", monospacedDetail: true) {
                                Button("Forget") { core.forget(device, canControlThisMac: true) }
                                    .buttonStyle(SecondaryButtonStyle(destructive: true))
                            }
                        } else {
                            HostRow(icon: icon, device: device)
                        }
                    }
                }
            }
        }
    }
}

/// A Mac this one controls: what this Mac calls it, its own name and ID, its two addresses, and
/// Rename, Connect (the way chosen under Your Macs in Connect) and Forget.
private struct HostRow: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.openWindow) private var openWindow
    let icon: String
    let device: PairedDevice
    @State private var renaming = false

    var body: some View {
        HStack(spacing: 12) {
            IconBadge(systemName: icon, tint: .lkAccent)
            VStack(alignment: .leading, spacing: 4) {
                Text(device.displayName)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Color.lkText)
                Group {
                    if device.alias != nil {
                        Text("\(device.name) · ") + Text("Device ID \(device.deviceId)").font(.system(size: 12, design: .monospaced))
                    } else {
                        Text("Device ID \(device.deviceId)").font(.system(size: 12, design: .monospaced))
                    }
                }
                .font(.system(size: 12))
                .foregroundStyle(Color.lkSecondary)
                .textSelection(.enabled)
                MacAddresses(device: device)
            }
            Spacer(minLength: 8)
            HStack(spacing: 8) {
                Button("Rename") { renaming = true }
                    .buttonStyle(SecondaryButtonStyle())
                    .renamePopover(device: device, isPresented: $renaming)
                if device.canConnect(device.connection) {
                    Button("Connect") { openWindow(id: "viewer", value: core.connect(to: device)) }
                        .buttonStyle(SecondaryButtonStyle())
                        .help(connectHelp(device))
                }
                Button("Forget") { core.forget(device, canControlThisMac: false) }
                    .buttonStyle(SecondaryButtonStyle(destructive: true))
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
    }
}
