import SwiftUI

struct ConnectView: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.openWindow) private var openWindow
    @State private var target = ""
    @FocusState private var fieldFocused: Bool

    var body: some View {
        Page {
            PageHeader(
                title: "Connect to a Mac",
                subtitle: "Enter the address shown under This Mac in LanKVM on the other computer."
            )

            Card {
                HStack(spacing: 10) {
                    Image(systemName: "network")
                        .foregroundStyle(Color.lkSecondary)
                    TextField("192.168.1.20", text: $target)
                        .textFieldStyle(.plain)
                        .font(.system(size: 15))
                        .focused($fieldFocused)
                        .onSubmit(connect)
                    Button("Connect", action: connect)
                        .buttonStyle(PrimaryButtonStyle())
                        .disabled(trimmed.isEmpty)
                        .keyboardShortcut(.defaultAction)
                }
                .padding(.leading, 14)
                .padding(.trailing, 8)
                .padding(.vertical, 8)
            }

            if !core.recents.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    SectionLabel(title: "Recent")
                    Card {
                        ForEach(Array(core.recents.enumerated()), id: \.element.id) { index, recent in
                            if index > 0 { CardDivider() }
                            RecentRow(recent: recent) { open(recent.address, label: recent.name) }
                        }
                    }
                }
            }

            TipCard()
        }
        .onAppear { fieldFocused = true }
    }

    private var trimmed: String {
        target.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func connect() {
        guard !trimmed.isEmpty else { return }
        open(trimmed)
    }

    /// A recent Mac's window shows its name: its address may be "lankvm:" and a fingerprint.
    private func open(_ address: String, label: String? = nil) {
        let id = core.connect(to: address, label: label)
        openWindow(id: "viewer", value: id)
    }
}

private struct RecentRow: View {
    let recent: RecentHost
    let action: () -> Void
    @State private var hovering = false

    /// A paired Mac connected to from Paired Devices is "lankvm:" and its fingerprint: no address
    /// worth reading.
    private var paired: Bool { recent.address.hasPrefix("lankvm:") }

    var body: some View {
        Button(action: action) {
            CardRow(icon: "desktopcomputer", tint: .lkAccent, title: recent.name, detail: paired ? "Over the internet" : recent.address,
                    monospacedDetail: !paired) {
                Image(systemName: "arrow.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(hovering ? Color.lkAccent : Color.lkSecondary)
            }
            .contentShape(Rectangle())
            .background(hovering ? Color.lkText.opacity(0.03) : .clear)
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

private struct TipCard: View {
    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            IconBadge(systemName: "bolt.fill", tint: .lkAccent)
            VStack(alignment: .leading, spacing: 4) {
                Text("For the lowest latency")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(Color.lkText)
                Text("Connect the Macs you control over Ethernet, and use full screen (⌃⌘F) on the viewer for pixel-exact, sharp text.")
                    .font(.system(size: 12))
                    .foregroundStyle(Color.lkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.lkAccent.opacity(0.06), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}
