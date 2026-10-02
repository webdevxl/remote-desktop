import AppKit
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
                SectionLabel(title: "Viewing this Mac now")
                Card {
                    if core.host.viewers.isEmpty {
                        CardRow(icon: "eye.slash", title: "Nobody is connected")
                    }
                    ForEach(Array(core.host.viewers.enumerated()), id: \.element.id) { index, viewer in
                        if index > 0 { CardDivider() }
                        CardRow(icon: "eye", tint: .lkAccent, title: viewer.name, detail: viewer.address, monospacedDetail: true) {
                            Button("Disconnect") { core.kick(viewer) }
                                .buttonStyle(SecondaryButtonStyle(destructive: true))
                        }
                    }
                }
            }
        }
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
        case .some(true): "Paired Macs can see and control this screen. You'll be asked for a code the first time a new Mac connects."
        case .some(false): "macOS hasn't allowed LanKVM to record the screen yet, so other Macs can't view this one."
        case .none: "Checking Screen Recording permission…"
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
