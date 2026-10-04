import AppKit
import CLanKVM
import SwiftUI

extension StreamEngine {
    /// LK_ENGINE_* in lankvm.h.
    var code: UInt8 {
        let code = switch self {
        case .lankvm: LK_ENGINE_LANKVM
        case .sunshine: LK_ENGINE_SUNSHINE
        }
        return UInt8(code)
    }
}

/// The engine last used with a host, put back on the next connection (`engine.<hostId>` in the
/// defaults). Sunshine is written once the host streams with it at the user's request; LanKVM
/// (nothing written) as soon as the user picks it. The host's own changes (Sunshine quit, a
/// refusal on connecting) never touch it.
enum RememberedEngine {
    private static func key(_ hostId: String) -> String { "engine.\(hostId)" }

    static func load(_ hostId: String) -> StreamEngine {
        guard !hostId.isEmpty, let saved = UserDefaults.standard.string(forKey: key(hostId)) else { return .lankvm }
        return StreamEngine(rawValue: saved) ?? .lankvm
    }

    static func save(_ engine: StreamEngine, for hostId: String) {
        guard !hostId.isEmpty else { return }
        if engine == .lankvm {
            UserDefaults.standard.removeObject(forKey: key(hostId))
        } else {
            UserDefaults.standard.set(engine.rawValue, forKey: key(hostId))
        }
    }
}

/// The Engine menu: what streams the host's picture. LanKVM's own engine into this window, or
/// the host's Sunshine shown here by Moonlight (both installed separately; LanKVM's session stays
/// up for displays). In the toolbar, the session control's menu and the Control menu; the
/// checkmark shows the engine streaming now.
struct EngineMenu: View {
    /// Moonlight opens full screen rather than in a window (a setting).
    static let fullScreenKey = "moonlightFullScreen"

    @ObservedObject var session: SessionModel
    /// In the toolbar: the icon (its tooltip says the rest). In a menu: the title alone.
    var inToolbar = false
    @AppStorage(EngineMenu.fullScreenKey) private var fullScreen = false

    var body: some View {
        Menu {
            if case .connected(let info) = session.phase {
                items(info)
            }
        } label: {
            if inToolbar {
                Label("Engine", systemImage: "play.tv")
            } else {
                Text("Engine")
            }
        }
    }

    @ViewBuilder private func items(_ info: SessionInfo) -> some View {
        let host = "“\(info.hostName)”"
        let installed = lk_moonlight_installed()
        Section("Stream \(host) with") {
            check("LanKVM", on: session.engine == .lankvm) {
                CoreModel.shared.setEngine(.lankvm, for: session.id)
            }
            check("Sunshine + Moonlight", on: session.engine == .sunshine) {
                CoreModel.shared.setEngine(.sunshine, for: session.id)
            }
            .disabled(!installed || info.internet)
        }
        if !installed {
            Text("Install Moonlight (./install.sh or brew install --cask moonlight)")
        } else if info.internet {
            // The host keeps Sunshine's ports closed to the internet.
            Text("Sunshine streams only on the local network")
        }
        Divider()
        Toggle("Open Moonlight in Full Screen", isOn: $fullScreen)
            .disabled(!installed)
        Text("⌘Tab and other system keys then go to \(host). Used the next time you choose Sunshine + Moonlight.")
    }

    /// A menu item with a checkmark when it's the engine streaming now.
    private func check(_ title: String, on: Bool, action: @escaping () -> Void) -> some View {
        Toggle(title, isOn: Binding(get: { on }, set: { _ in action() }))
    }
}

/// Says what's happening to the engine, over the top of the screen: Sunshine starting on the
/// host (with a spinner), and why the session isn't on the engine asked for, or the host's news
/// about it, until dismissed.
struct EngineBanner: View {
    @ObservedObject var session: SessionModel
    let info: SessionInfo

    var body: some View {
        Group {
            if let asked = session.engineSwitch, asked.engine == .sunshine {
                BannerPill(icon: "play.tv", text: "Starting Sunshine on “\(info.hostName)”…", tint: .white, busy: true)
            }
            if let notice = session.engineNotice {
                BannerPill(icon: "exclamationmark.triangle", text: notice, tint: Color(hex: 0xE5B45A),
                           dismiss: { session.engineNotice = nil })
            }
        }
        .animation(.easeOut(duration: 0.15), value: session.engineSwitch)
        .animation(.easeOut(duration: 0.15), value: session.engineNotice)
    }
}

/// The window while the host's Sunshine streams: the picture is in Moonlight's window, so this
/// one says how Moonlight is doing, with ways to bring it forward, open it again, go back to
/// LanKVM's own stream, or disconnect. LanKVM's session stays up underneath (the Display menu
/// still works); its keyboard and mouse forwarding is off, as Moonlight has its own.
struct MoonlightView: View {
    @ObservedObject var session: SessionModel
    let info: SessionInfo
    let close: () -> Void

    private var host: String { "“\(info.hostName)”" }

    /// What the window says, from Moonlight's state.
    private enum Phase: Equatable {
        /// Going back to LanKVM's stream, waiting for the host.
        case leaving
        /// Nothing heard yet, or starting (again).
        case starting
        case pairing
        case streaming
        case closed
        case failed(String)
    }

    private var phase: Phase {
        if session.engineSwitch?.engine == .lankvm { return .leaving }
        switch session.moonlight?.state {
        case nil, .starting: return .starting
        case .pairing: return .pairing
        case .streaming: return .streaming
        case .ended: return .closed
        case .failed: return .failed(session.moonlight?.message ?? "")
        }
    }

    var body: some View {
        let phase = phase
        ZStack {
            Color.lkBackground
            VStack(spacing: 14) {
                IconBadge(systemName: icon(phase), tint: tint(phase))
                    .scaleEffect(1.4)
                Text("Streaming in Moonlight")
                    .font(.system(size: 22, design: .serif))
                    .foregroundStyle(Color.lkText)
                VStack(spacing: 6) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        if busy(phase) {
                            ProgressView()
                                .controlSize(.small)
                                .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + 4 }
                        }
                        Text(stateLine(phase))
                            .font(.system(size: 14, weight: .medium))
                            .foregroundStyle(Color.lkText)
                            .multilineTextAlignment(.center)
                            .textSelection(.enabled)
                    }
                    Text(detail(phase))
                        .font(.system(size: 13))
                        .foregroundStyle(Color.lkSecondary)
                        .multilineTextAlignment(.center)
                }
                .frame(maxWidth: 440)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityElement(children: .combine)
                buttons(phase)
                    .padding(.top, 4)
            }
            .padding(32)
            .animation(.easeOut(duration: 0.15), value: phase)
        }
        .overlay(alignment: .top) {
            VStack(spacing: 8) {
                DisplayBanner(session: session, controls: session.sessionControl, info: info)
                EngineBanner(session: session, info: info)
            }
            .padding(.top, 14)
            .padding(.horizontal, 16)
        }
        .overlay(alignment: .bottom) {
            if let text = session.toast {
                Text(text)
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 9)
                    .background(.black.opacity(0.72), in: Capsule())
                    .padding(.bottom, 28)
                    .allowsHitTesting(false)
                    .transition(.opacity)
            }
        }
        .animation(.easeOut(duration: 0.2), value: session.toast)
    }

    @ViewBuilder private func buttons(_ phase: Phase) -> some View {
        HStack(spacing: 10) {
            switch phase {
            case .closed, .failed:
                Button("Open Moonlight Again") { CoreModel.shared.openMoonlight(for: session.id) }
                    .buttonStyle(PrimaryButtonStyle())
                    .keyboardShortcut(.defaultAction)
                    .help("Open Moonlight on “\(info.hostName)” again")
            case .starting, .pairing, .streaming, .leaving:
                Button("Show Moonlight") { CoreModel.shared.showMoonlight(for: session.id) }
                    .buttonStyle(PrimaryButtonStyle())
                    .keyboardShortcut(.defaultAction)
                    .disabled(phase == .leaving || session.moonlight?.pid == nil)
                    .help("Bring Moonlight's window to the front")
            }
            Button("Use LanKVM’s Stream") { CoreModel.shared.setEngine(.lankvm, for: session.id) }
                .buttonStyle(SecondaryButtonStyle())
                .disabled(phase == .leaving)
                // (The secondary style doesn't dim by itself.)
                .opacity(phase == .leaving ? 0.5 : 1)
                .help("Show “\(info.hostName)” in this window again, with LanKVM's own engine")
            Button("Disconnect", action: close)
                .buttonStyle(SecondaryButtonStyle())
                .help("Disconnect from this Mac; Moonlight closes too")
        }
    }

    private func stateLine(_ phase: Phase) -> String {
        switch phase {
        case .leaving: "Switching back to LanKVM’s stream…"
        case .starting: "Starting Moonlight…"
        case .pairing: "Pairing Moonlight with \(host)…"
        case .streaming: "Moonlight shows \(host)."
        case .closed: "Moonlight closed."
        case .failed(let message): message.isEmpty ? "Moonlight couldn't show \(host)." : message
        }
    }

    private func detail(_ phase: Phase) -> String {
        switch phase {
        case .leaving: "\(host) stops Sunshine, and this window shows its screen again."
        case .pairing: "Moonlight shows a PIN. LanKVM passes it to \(host), so there's nothing to type."
        case .closed: "Open it again to go on streaming \(host), or use LanKVM's stream in this window."
        case .failed: "Try again, or use LanKVM's stream in this window."
        case .starting, .streaming:
            "The picture is in Moonlight's window. It also sends your keyboard and mouse when \(host) lets paired Macs control it. The Display menu here still changes what \(host) shows."
        }
    }

    private func busy(_ phase: Phase) -> Bool {
        phase == .leaving || phase == .starting || phase == .pairing
    }

    private func icon(_ phase: Phase) -> String {
        switch phase {
        case .leaving: "arrow.uturn.backward"
        case .pairing: "lock.open"
        case .closed: "pause.circle"
        case .failed: "exclamationmark.triangle"
        case .starting, .streaming: "play.tv"
        }
    }

    private func tint(_ phase: Phase) -> Color {
        switch phase {
        case .streaming: .lkAccent
        case .failed: .lkWarning
        default: .lkSecondary
        }
    }
}
