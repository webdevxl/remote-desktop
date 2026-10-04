import AppKit
import CLanKVM
import QuartzCore
import SwiftUI

/// One window per remote Mac.
struct ViewerView: View {
    @EnvironmentObject private var core: CoreModel
    @Environment(\.dismiss) private var dismiss
    @State private var sessionId: UInt64

    init(sessionId: UInt64) {
        _sessionId = State(initialValue: sessionId)
    }

    var body: some View {
        Group {
            if let session = core.session(sessionId) {
                ViewerContent(session: session, controls: session.sessionControl, reconnect: reconnect, close: { dismiss() })
            } else {
                EndedView(target: nil, error: nil, wasConnected: true, reconnect: nil, close: { dismiss() })
                    // A window macOS restored from the last run: its session is long gone.
                    .onAppear { dismiss() }
            }
        }
        .frame(minWidth: 480, minHeight: 320)
        .tint(.lkAccent)
        .onDisappear { core.disconnect(sessionId) }
    }

    private func reconnect(_ target: String) {
        core.disconnect(sessionId)
        sessionId = core.connect(to: target)
    }
}

private struct ViewerContent: View {
    @EnvironmentObject private var core: CoreModel
    @ObservedObject var session: SessionModel
    @ObservedObject var controls: SessionControlModel
    let reconnect: (String) -> Void
    let close: () -> Void
    @AppStorage("showStats") private var showStats = true

    var body: some View {
        ZStack {
            switch session.phase {
            case .connecting:
                ConnectingView(target: session.target, cancel: close)
            case .needsPin:
                ZStack {
                    Color.lkBackground
                    PinEntryView(
                        hostLabel: session.target,
                        onSubmit: { core.submitPin($0, for: session.id) },
                        onCancel: close
                    )
                }
            case .connected(let info):
                if case .switching(_, let label, fromConnect: true) = session.display {
                    // The display used last time is being set up: the host's own screen would only
                    // flash by meanwhile.
                    ConnectingView(target: info.hostName, detail: "Setting up a \(label) display…", cancel: close)
                } else if let asked = session.engineSwitch, asked.fromConnect, asked.engine == .sunshine {
                    // Likewise while Sunshine + Moonlight, used last time, starts.
                    ConnectingView(target: info.hostName, detail: "Starting Sunshine for Moonlight…", cancel: close)
                } else if session.streamsInMoonlight {
                    // The picture is in Moonlight's window. Without the remote screen here, nothing
                    // goes to the host from this window's keyboard and mouse, and the session
                    // control is gone with the other overlays.
                    MoonlightView(session: session, info: info, close: close)
                } else {
                    RemoteScreen(session: session, info: info, fillsScreen: controls.fillsThisScreen(info.display),
                                 toggleControl: toggleControl)
                        .background(Color.black)
                        .overlay {
                            ScreenOverlays(session: session, controls: controls, info: info,
                                           retry: { core.setControl(true, for: session.id) },
                                           takeOver: { core.setControl(true, for: session.id, takeOver: true) })
                        }
                }
            case .ended(let error):
                EndedView(target: session.target, error: error, wasConnected: session.wasConnected,
                          reconnect: { reconnect(session.target) }, close: close)
            }
        }
        .navigationTitle(title)
        .navigationSubtitle(subtitle)
        // The Control menu acts on the viewer window in front.
        .focusedSceneObject(session)
        .focusedSceneObject(controls)
        // A real sheet: while it's open, typing goes to its fields, not to the remote Mac.
        .sheet(item: $controls.customDisplay) { draft in
            CustomDisplaySheet(draft: draft, hostName: session.hostName, apply: controls.applyCustomDisplay,
                               cancel: { controls.customDisplay = nil })
        }
        .toolbar {
            if case .connected(let info) = session.phase {
                ToolbarItemGroup(placement: .principal) {
                    Picker("Mode", selection: Binding(get: { session.mode }, set: { setMode($0) })) {
                        Label("View", systemImage: "eye").tag(SessionModel.Mode.view)
                        Label("Control", systemImage: "cursorarrow.rays").tag(SessionModel.Mode.control)
                    }
                    .pickerStyle(.segmented)
                    .labelStyle(.titleAndIcon)
                    .help(session.streamsInMoonlight ? "Moonlight's window has its own keyboard and mouse"
                        : "View only, or control this Mac with your mouse and keyboard. While controlling, ⌃⌥⌘ releases your keyboard and mouse and takes them back.")
                    .disabled(session.streamsInMoonlight)
                    DisplayMenu(session: session, controls: controls, inToolbar: true)
                        .help("Show “\(info.hostName)” at the size of this screen")
                    EngineMenu(session: session, inToolbar: true)
                        .help("Stream “\(info.hostName)” with LanKVM, or with Sunshine shown in Moonlight")
                }
                ToolbarItemGroup(placement: .primaryAction) {
                    Toggle(isOn: $showStats) {
                        Label("Statistics", systemImage: "gauge.with.dots.needle.33percent")
                    }
                    .help(session.streamsInMoonlight ? "Statistics are for LanKVM’s own stream" : "Show latency and bandwidth")
                    .disabled(session.streamsInMoonlight)
                    Button(action: close) {
                        Label("Disconnect", systemImage: "xmark.circle")
                    }
                    .help("Disconnect from this Mac")
                }
            }
        }
    }

    private var title: String {
        if case .connected(let info) = session.phase { return info.hostName }
        return session.target
    }

    private var subtitle: String {
        guard case .connected = session.phase else { return "" }
        if session.streamsInMoonlight { return "Streaming in Moonlight" }
        switch session.control {
        case .active where controls.isReleased && session.sameMachine: return "Released (test mode) · click the screen to control"
        case .active where controls.isReleased: return "Released · click the screen to control"
        case .active where session.sameMachine: return "Controlling this Mac (test mode) · press ⌃⌥⌘ to release"
        case .active: return "Controlling · press ⌃⌥⌘ to release"
        case .requesting: return "Asking for control…"
        default: return "Viewing"
        }
    }

    private func setMode(_ mode: SessionModel.Mode) {
        core.setControl(mode == .control, for: session.id)
    }

    private func toggleControl() {
        setMode(session.mode == .control ? .view : .control)
    }
}

/// Everything drawn over the remote picture: statistics, the control banner and the gesture hint,
/// toasts, and the session control on top. The others keep clear of where the session control is
/// docked, and the statistics of the banners.
struct ScreenOverlays: View {
    @ObservedObject var session: SessionModel
    @ObservedObject var controls: SessionControlModel
    let info: SessionInfo
    /// Statistics to show instead of the session's (snapshots).
    var sampleStats: SessionStats?
    /// Whether Accessibility is granted, instead of asking macOS (snapshots).
    var sampleTrusted: Bool?
    var retry: () -> Void = {}
    var takeOver: () -> Void = {}
    @AppStorage("showStats") private var showStats = true
    @AppStorage(SessionControlModel.visibleKey) private var showSessionControl = true
    @State private var hudSize = CGSize.zero
    @State private var bannerSize = CGSize.zero
    @State private var toastSize = CGSize.zero

    /// Space kept between overlays.
    private static let gap: CGFloat = 8

    var body: some View {
        let screen = controls.screenSize
        let reserved = controls.isShown(enabled: showSessionControl) ? controls.reservedFrame : .null
        let banner = bannerFrame(screen: screen, reserved: reserved)
        let hud = hudFrame(reserved: reserved, banner: banner)
        let toast = CGRect(x: (screen.width - toastSize.width) / 2, y: screen.height - 28 - toastSize.height,
                           width: toastSize.width, height: toastSize.height)
        Color.clear
            .overlay {
                // The picture is about to change: dimmed meanwhile (the pointer rests too).
                if session.display.isSwitching {
                    Color.black.opacity(0.35)
                        .allowsHitTesting(false)
                        .transition(.opacity)
                }
            }
            .overlay(alignment: .topLeading) {
                if showStats {
                    // Never in the way of the remote Mac's menu bar.
                    StatsHUD(sessionId: session.id, info: info, sample: sampleStats)
                        .onGeometryChange(for: CGSize.self) { $0.size } action: { hudSize = $0 }
                        .padding(.leading, 12)
                        .padding(.top, hud.minY)
                        .animation(.easeOut(duration: 0.18), value: hud.minY)
                        .allowsHitTesting(false)
                }
            }
            .overlay(alignment: .top) {
                VStack(spacing: Self.gap) {
                    DisplayBanner(session: session, controls: controls, info: info)
                    EngineBanner(session: session, info: info)
                    ControlBanner(session: session, hostName: info.hostName, retry: retry, takeOver: takeOver)
                    GestureAccessHint(session: session, hostName: info.hostName, sampleTrusted: sampleTrusted)
                }
                .onGeometryChange(for: CGSize.self) { $0.size } action: { bannerSize = $0 }
                .padding(.top, banner.minY)
            }
            .overlay(alignment: .bottom) {
                if let text = session.toast {
                    Text(text)
                        .font(.system(size: 13, weight: .medium))
                        .foregroundStyle(.white)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 9)
                        .background(.black.opacity(0.72), in: Capsule())
                        .onGeometryChange(for: CGSize.self) { $0.size } action: { toastSize = $0 }
                        .padding(.bottom, overlaps(reserved, toast) ? screen.height - reserved.minY + Self.gap : 28)
                        .allowsHitTesting(false)
                        .transition(.opacity)
                }
            }
            .overlay {
                SessionControl(session: session, model: controls)
            }
            .animation(.easeOut(duration: 0.2), value: session.toast)
            .animation(.easeOut(duration: 0.18), value: controls.placement)
            .animation(.easeOut(duration: 0.2), value: session.display.isSwitching)
    }

    /// The banners, centred at the top, below the session control when it's there.
    private func bannerFrame(screen: CGSize, reserved: CGRect) -> CGRect {
        var banner = CGRect(x: (screen.width - bannerSize.width) / 2, y: 14, width: bannerSize.width, height: bannerSize.height)
        if overlaps(reserved, banner) { banner.origin.y = reserved.maxY + Self.gap }
        return banner
    }

    /// The statistics, top left, below the session control or the banners when they'd overlap
    /// (a narrow window).
    private func hudFrame(reserved: CGRect, banner: CGRect) -> CGRect {
        var hud = CGRect(x: 12, y: 12, width: hudSize.width, height: hudSize.height)
        if overlaps(reserved, hud) { hud.origin.y = reserved.maxY + Self.gap }
        if overlaps(banner, hud) { hud.origin.y = banner.maxY + Self.gap }
        return hud
    }

    private func overlaps(_ reserved: CGRect, _ frame: CGRect) -> Bool {
        !reserved.isNull && frame.width > 0 && frame.height > 0 && reserved.intersects(frame.insetBy(dx: -Self.gap, dy: -Self.gap))
    }
}

/// Says why control isn't happening (asked, refused, taken back), over the top of the screen,
/// with the action that helps when there is one.
struct ControlBanner: View {
    @ObservedObject var session: SessionModel
    let hostName: String
    var retry: () -> Void = {}
    var takeOver: () -> Void = {}
    @State private var dismissed: String?

    var body: some View {
        Group {
            switch session.control {
            case .requesting:
                pill(icon: "hourglass", text: "Asking \(hostName) for control…", tint: .white)
            case .refused(let reason, let message) where dismissed != message:
                pill(icon: "hand.raised", text: message, tint: Color(hex: 0xE5B45A), action: action(for: reason)) {
                    dismissed = message
                }
            case .active where session.sameMachine:
                pill(icon: "exclamationmark.triangle", text: "Controlling this same Mac (test mode). Pointer and keyboard are shared; press ⌃⌥⌘ to release them.",
                     tint: Color(hex: 0xE5B45A))
            default:
                EmptyView()
            }
        }
        .animation(.easeOut(duration: 0.15), value: session.control)
        // A new request deserves its answer shown, even if it reads like one dismissed before.
        .onChange(of: session.control) { _, new in
            if new == .requesting { dismissed = nil }
        }
    }

    private func action(for reason: Int) -> (String, () -> Void)? {
        switch reason {
        case ControlReason.needsPermission, ControlReason.permissionLost: ("Try Again", retry)
        case ControlReason.inUse: ("Take Over", takeOver)
        case ControlReason.takenOver: ("Take Back", takeOver)
        case ControlReason.stoppedByHost, ControlReason.testTimeout, ControlReason.badInput: ("Control Again", retry)
        default: nil
        }
    }

    private func pill(icon: String, text: String, tint: Color, action: (String, () -> Void)? = nil,
                      dismiss: (() -> Void)? = nil) -> some View {
        BannerPill(icon: icon, text: text, tint: tint, action: action, dismiss: dismiss)
    }
}

/// Says what's happening to the display the session shows: switching (with a spinner), why the
/// host didn't show what was asked for or took it away, a display offered instead of put back,
/// and video this Mac can't show, each with the action that helps when there is one.
struct DisplayBanner: View {
    @ObservedObject var session: SessionModel
    let controls: SessionControlModel
    let info: SessionInfo

    var body: some View {
        Group {
            status
            if let error = session.streamError {
                BannerPill(icon: "exclamationmark.triangle", text: error, tint: Self.warning, action: ownScreen,
                           dismiss: { session.streamError = nil })
            }
        }
        .animation(.easeOut(duration: 0.15), value: session.display)
        .animation(.easeOut(duration: 0.15), value: session.streamError)
    }

    private var host: String { "“\(info.hostName)”" }

    /// Video this Mac can't show: the host's own screen may do (one of its displays won't).
    private var ownScreen: (String, () -> Void)? {
        guard info.display.kind == .virtual else { return nil }
        return ("Use \(host)’s Own Screen", controls.showMainDisplay)
    }

    private static let warning = Color(hex: 0xE5B45A)

    @ViewBuilder private var status: some View {
        switch session.display {
        case .switching(_, let label, _):
            BannerPill(icon: "display", text: "Switching \(host) to \(label)…", tint: .white, busy: true)
        case .failed(let reason, let message, let retry):
            BannerPill(icon: "exclamationmark.triangle", text: message, tint: Self.warning, action: action(for: reason, retry: retry),
                       dismiss: { session.display = .idle })
        case .offered(let display):
            BannerPill(icon: "display", text: "Use \(display.sizeText) on \(host)?", tint: .white,
                       action: ("Use", { controls.showDisplay(display) }), dismiss: { session.display = .idle })
        case .idle:
            EmptyView()
        }
    }

    private func action(for reason: Int, retry: RemoteDisplay?) -> (String, () -> Void)? {
        guard let retry else { return nil }
        switch reason {
        case DisplayReason.failed: return ("Try Again", { controls.showDisplay(retry) })
        case DisplayReason.removedByHost, DisplayReason.gone:
            guard retry.kind == .virtual else { return nil }
            return ("Use \(retry.sizeText) Again", { controls.showDisplay(retry) })
        case DisplayReason.inUse:
            // Another Mac controls the host: the main display isn't this Mac's, but one next to the
            // host's screen is.
            guard retry.kind == .virtual, retry.arrangement != .extend else { return nil }
            var extended = retry
            extended.arrangement = .extend
            return ("Put It Next to “\(info.hostName)”’s Screen", {
                controls.nextArrangement = .extend
                controls.showDisplay(extended)
            })
        default: return nil
        }
    }
}

/// While controlling, Mission Control and Spaces swipes still act on this Mac until LanKVM may
/// take them (Accessibility, here on this Mac). Says so, with the way to fix it, until fixed or
/// dismissed for this window. The other gestures need nothing.
struct GestureAccessHint: View {
    @ObservedObject var session: SessionModel
    let hostName: String
    @AppStorage(TrackpadGestures.defaultsKey) private var sendTrackpadGestures = true
    /// Whether Accessibility is granted, as last checked.
    @State private var trusted: Bool
    private let sampleTrusted: Bool?

    /// Test copies (scripts/e2e-control.sh) never offer permissions.
    private static let noPrompts = ProcessInfo.processInfo.environment["LANKVM_NO_PROMPTS"] == "1"

    /// `sampleTrusted`: for snapshots, whether Accessibility is granted (nil: ask macOS).
    init(session: SessionModel, hostName: String, sampleTrusted: Bool? = nil) {
        self.session = session
        self.hostName = hostName
        self.sampleTrusted = sampleTrusted
        _trusted = State(initialValue: sampleTrusted ?? true)
    }

    var body: some View {
        Group {
            if relevant && !trusted {
                BannerPill(icon: "hand.draw",
                           text: "Mission Control and Spaces swipes still act on this Mac. To send them to “\(hostName)”, allow LanKVM in Accessibility.",
                           tint: Color(hex: 0xE5B45A),
                           action: ("Open Settings", { CoreModel.shared.requestControlPermission() }),
                           dismiss: { session.gestureHintDismissed = true })
                    .transition(.opacity)
            }
        }
        .animation(.easeOut(duration: 0.15), value: relevant && !trusted)
        // macOS reports a grant while LanKVM runs, and a revocation: the hint goes as soon as it's
        // given, and comes back if it's taken away (DockGestures lets go of the swipes then).
        .task(id: relevant) {
            guard relevant, sampleTrusted == nil else { return }
            while !Task.isCancelled {
                let now = AXIsProcessTrusted()
                if now != trusted { trusted = now }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private var relevant: Bool {
        session.isControlling && sendTrackpadGestures && !session.sameMachine && !session.gestureHintDismissed && !Self.noPrompts
    }
}

/// A message over the top of the remote screen, legible on any picture: an icon, the text, and
/// the action that helps and a close button when there are some.
struct BannerPill: View {
    let icon: String
    let text: String
    let tint: Color
    var action: (String, () -> Void)?
    var dismiss: (() -> Void)?
    /// Something is under way: a small spinner instead of the icon.
    var busy = false

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            if busy {
                ProgressView()
                    .controlSize(.small)
                    .scaleEffect(0.8)
                    .frame(width: 14, height: 14)
                    .alignmentGuide(.firstTextBaseline) { $0[VerticalAlignment.center] + 4 }
            } else {
                Image(systemName: icon).foregroundStyle(tint)
            }
            Text(text)
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.white)
                .multilineTextAlignment(.leading)
                .frame(maxWidth: 460, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
            if let (title, perform) = action {
                Button(title, action: perform)
                    .buttonStyle(.plain)
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Color.lkAccent)
            }
            if let dismiss {
                Button(action: dismiss) {
                    Image(systemName: "xmark").font(.system(size: 10, weight: .bold)).foregroundStyle(.white.opacity(0.7))
                }
                .buttonStyle(.plain)
                .help("Dismiss")
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .background(.black.opacity(0.72), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(.white.opacity(0.1)))
        .environment(\.colorScheme, .dark)
    }
}

struct ConnectingView: View {
    let target: String
    /// A second line: what's being set up once connected.
    var detail: String?
    let cancel: () -> Void

    var body: some View {
        ZStack {
            Color.lkBackground
            VStack(spacing: 14) {
                ProgressView().controlSize(.large)
                VStack(spacing: 4) {
                    Text("Connecting to \(target)…")
                        .font(.system(size: 15, design: .serif))
                        .foregroundStyle(Color.lkText)
                    if let detail {
                        Text(detail)
                            .font(.system(size: 13))
                            .foregroundStyle(Color.lkSecondary)
                    }
                }
                Button("Cancel", action: cancel)
                    .buttonStyle(SecondaryButtonStyle())
                    .keyboardShortcut(.cancelAction)
            }
        }
    }
}

struct EndedView: View {
    let target: String?
    let error: String?
    var wasConnected = false
    let reconnect: (() -> Void)?
    let close: () -> Void

    var body: some View {
        ZStack {
            Color.lkBackground
            VStack(spacing: 14) {
                IconBadge(systemName: error == nil ? "checkmark.circle" : "exclamationmark.triangle",
                          tint: error == nil ? .lkSecondary : .lkWarning)
                    .scaleEffect(1.4)
                Text(error == nil ? "Disconnected" : wasConnected ? "Connection lost" : "Couldn't connect")
                    .font(.system(size: 22, design: .serif))
                    .foregroundStyle(Color.lkText)
                if let error {
                    Text(error)
                        .font(.system(size: 13))
                        .foregroundStyle(Color.lkSecondary)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 420)
                        .textSelection(.enabled)
                }
                HStack(spacing: 10) {
                    Button("Close", action: close)
                        .buttonStyle(SecondaryButtonStyle())
                        .keyboardShortcut(.cancelAction)
                    if let reconnect {
                        Button("Reconnect", action: reconnect)
                            .buttonStyle(PrimaryButtonStyle())
                            .keyboardShortcut(.defaultAction)
                    }
                }
                .padding(.top, 4)
            }
            .padding(32)
        }
    }
}

/// Live latency and bandwidth, refreshed twice a second.
struct StatsHUD: View {
    @EnvironmentObject private var core: CoreModel
    let sessionId: UInt64
    let info: SessionInfo
    @State private var stats: SessionStats?
    private let timer = Timer.publish(every: 0.5, on: .main, in: .common).autoconnect()

    init(sessionId: UInt64, info: SessionInfo, sample: SessionStats? = nil) {
        self.sessionId = sessionId
        self.info = info
        _stats = State(initialValue: sample)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text("Latency")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                Text(verbatim: "\(String(info.width))×\(String(info.height)) · \(info.codec.uppercased())\(info.internet ? " · Internet" : "")")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                Text(ms(stats?.totalMs))
                    .font(.system(size: 30, weight: .semibold, design: .rounded))
                    .foregroundStyle(latencyColor)
                    .contentTransition(.numericText())
                Text("ms capture → screen")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            }
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 4) {
                row("Capture", stats?.captureMs)
                row("Encode", stats?.encodeMs)
                row("Network", stats?.networkMs)
                row("Decode", stats?.decodeMs)
                row("Display", stats?.displayMs)
                if let input = stats?.inputMs {
                    row("Input → remote", input)
                }
            }
            Divider().opacity(0.5)
            Text(footer)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .frame(width: 290)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(.white.opacity(0.08)))
        .environment(\.colorScheme, .dark)
        .onReceive(timer) { _ in if sessionId != 0 { stats = core.stats(for: sessionId) } }
        .onAppear { if sessionId != 0 { stats = core.stats(for: sessionId) } }
    }

    private func row(_ name: String, _ value: Double?) -> some View {
        GridRow {
            Text(name).font(.system(size: 12)).foregroundStyle(.secondary)
            Text("\(ms(value)) ms").font(.system(size: 12, design: .monospaced)).foregroundStyle(.primary)
        }
    }

    private func ms(_ v: Double?) -> String {
        v.map { String(format: "%.1f", $0) } ?? "–"
    }

    private var latencyColor: Color {
        guard let total = stats?.totalMs else { return .primary }
        return total < 25 ? Color(hex: 0x7FCB8D) : total < 50 ? Color(hex: 0xE5B45A) : Color(hex: 0xE5735A)
    }

    private var footer: String {
        guard let s = stats else { return "–" }
        let rtt = s.rttMs.map { String(format: "%.1f", $0) } ?? "–"
        var text = String(format: "%.0f fps · %.1f Mbit/s · RTT %@ ms", s.fps, s.mbps, rtt)
        if s.framesLost > 0 { text += " · lost \(s.framesLost)" }
        return text
    }
}

/// Hosts the CAMetalLayer the Rust core renders into, on its own thread, and (in Control mode)
/// turns the mouse and keyboard into input for the remote Mac.
private struct RemoteScreen: NSViewRepresentable {
    @ObservedObject var session: SessionModel
    let info: SessionInfo
    /// It shows a display that fills this window's screen.
    let fillsScreen: Bool
    let toggleControl: () -> Void

    func makeNSView(context: Context) -> MetalHostView {
        MetalHostView(sessionId: session.id)
    }

    func updateNSView(_ view: MetalHostView, context: Context) {
        view.configure(session: session, frameSize: CGSize(width: Int(info.width), height: Int(info.height)),
                       fillsScreen: fillsScreen, toggleControl: toggleControl)
    }

    static func dismantleNSView(_ view: MetalHostView, coordinator: ()) {
        view.detach()
    }
}

final class MetalHostView: NSView {
    let sessionId: UInt64
    private let metalLayer = CAMetalLayer()
    private var attached = false
    private lazy var input = InputForwarder(sessionId: sessionId, view: self)
    private weak var session: SessionModel?
    private var presentation: ImmersivePresentation?

    init(sessionId: UInt64) {
        self.sessionId = sessionId
        super.init(frame: .zero)
        metalLayer.isOpaque = true
        metalLayer.backgroundColor = NSColor.black.cgColor
        wantsLayer = true
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .mouseEnteredAndExited, .cursorUpdate, .activeInKeyWindow, .inVisibleRect, .enabledDuringMouseDrag],
            owner: self, userInfo: nil))
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func makeBackingLayer() -> CALayer { metalLayer }

    /// Called whenever SwiftUI updates the view (mode, control or display state changed).
    func configure(session: SessionModel, frameSize: CGSize, fillsScreen: Bool, toggleControl: @escaping () -> Void) {
        if self.session !== session {
            self.session = session
            input.attach(to: session)
            session.cursor.onChange = { [weak self] in self?.showRemoteCursor() }
            // Full screen hides this Mac's menu bar and Dock exactly while input goes remote,
            // however forwarding starts or stops (focus, sleep, another Space, Release...).
            input.onForwardingChanged = { [weak self] on in
                self?.presentation?.update(immersive: on)
                self?.showRemoteCursor()
            }
        }
        session.sessionControl.attach(forwarder: input, window: window)
        // The new picture's size first: the pointer goes back to work over it.
        input.frameSize = frameSize
        input.pointerSuspended = session.display.isSwitching
        input.onEscape = toggleControl
        input.update()
        presentation?.update(immersive: input.isForwarding, fillsScreen: fillsScreen)
        window?.invalidateCursorRects(for: self)
        showRemoteCursor()
    }

    private var controlling: Bool { session?.isControlling == true }

    /// Whether a point (view coordinates) is over the session control. The mouse there is this
    /// Mac's: moves, clicks and scrolls aren't sent to the remote Mac (keys still are).
    private func overSessionControl(_ point: NSPoint) -> Bool {
        session?.sessionControl.excludes(CGPoint(x: point.x, y: bounds.height - point.y)) ?? false
    }

    private func overSessionControl(_ event: NSEvent) -> Bool {
        overSessionControl(convert(event.locationInWindow, from: nil))
    }

    override var acceptsFirstResponder: Bool { true }
    override var isOpaque: Bool { true }
    // The click that brings the window to the front stays here; the next one goes remote.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { false }
    // A drag on the remote screen must never move this window.
    override var mouseDownCanMoveWindow: Bool { false }

    // MARK: Mouse (keys go through InputForwarder's event monitor)

    override func mouseMoved(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        // Tracking areas aren't covered by overlays: this sees moves over the session control too.
        session?.sessionControl.pointerMoved(to: CGPoint(x: point.x, y: bounds.height - point.y))
        guard controlling else { return super.mouseMoved(with: event) }
        // Over the session control the remote pointer stays where it left the picture.
        if !overSessionControl(point) {
            input.mouseMoved(event)
            // Hidden while the remote user types: show it again as soon as the mouse moves
            // (hidden again if the host doesn't confirm).
            if session?.cursor.state == .hidden { session?.cursor.unhide() }
        }
        showRemoteCursor(at: point)
    }
    override func mouseExited(with event: NSEvent) {
        session?.sessionControl.pointerMoved(to: nil)
        super.mouseExited(with: event)
    }
    override func mouseDragged(with event: NSEvent) { controlling ? drag(event) : super.mouseDragged(with: event) }
    override func rightMouseDragged(with event: NSEvent) { controlling ? drag(event) : super.rightMouseDragged(with: event) }
    override func otherMouseDragged(with event: NSEvent) { controlling ? drag(event) : super.otherMouseDragged(with: event) }
    override func mouseDown(with event: NSEvent) { controlling ? press(event) : super.mouseDown(with: event) }
    override func mouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.mouseUp(with: event) }
    override func rightMouseDown(with event: NSEvent) { controlling ? press(event) : super.rightMouseDown(with: event) }
    override func rightMouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.rightMouseUp(with: event) }
    override func otherMouseDown(with event: NSEvent) { controlling ? press(event) : super.otherMouseDown(with: event) }
    override func otherMouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.otherMouseUp(with: event) }
    override func scrollWheel(with event: NSEvent) {
        guard controlling else { return super.scrollWheel(with: event) }
        if !overSessionControl(event) { input.scroll(event) }
    }
    // Pinch, rotate, smart zoom and page swipes act on the remote Mac (or nowhere), never here.
    override func magnify(with event: NSEvent) { controlling ? gesture(event) : super.magnify(with: event) }
    override func rotate(with event: NSEvent) { controlling ? gesture(event) : super.rotate(with: event) }
    override func smartMagnify(with event: NSEvent) { controlling ? gesture(event) : super.smartMagnify(with: event) }
    override func swipe(with event: NSEvent) { controlling ? gesture(event) : super.swipe(with: event) }
    // Ctrl-click is a click with Control held, for the remote Mac to interpret.
    override func menu(for event: NSEvent) -> NSMenu? { controlling ? nil : super.menu(for: event) }

    /// A press while controlling: none goes remote next to the session control (its margin), and
    /// while released, one on the screen takes the keyboard and mouse back instead.
    private func press(_ event: NSEvent) {
        if overSessionControl(event) || input.resumeOnClick(event) { return }
        input.mouseButton(event, down: true)
    }

    /// A drag on the remote screen goes on even across the session control; without a button
    /// pressed on the screen it's only a move, which stops at the control like other moves.
    private func drag(_ event: NSEvent) {
        if !input.holdsButtons && overSessionControl(event) { return }
        input.mouseMoved(event)
    }

    /// A gesture while controlling goes to the remote Mac with Send Trackpad Gestures on, unless
    /// it starts on the session control.
    private func gesture(_ event: NSEvent) {
        input.gesture(event, mayBegin: TrackpadGestures.enabled && !overSessionControl(event))
    }

    // MARK: Cursor

    override func cursorUpdate(with event: NSEvent) {
        showRemoteCursor(at: convert(event.locationInWindow, from: nil))
    }

    /// Over the picture while input goes there: the remote Mac's cursor. Elsewhere, or released
    /// (the host draws its cursor in the video then): the arrow.
    private func showRemoteCursor(at point: NSPoint? = nil) {
        guard let window, window.isKeyWindow else { return }
        let p = point ?? convert(window.mouseLocationOutsideOfEventStream, from: nil)
        guard bounds.contains(p) else { return }
        let remote = session?.cursor.current
        if overSessionControl(p) {
            // This Mac's pointer on the control, never the remote one (it may be invisible). The
            // control sets its own hand over the grip, so only replace what was set here.
            if let remote, NSCursor.current === remote { NSCursor.arrow.set() }
        } else if input.isForwarding, let remote, input.pictureRect()?.contains(p) ?? true {
            if NSCursor.current !== remote { remote.set() }
        } else if NSCursor.current !== NSCursor.arrow {
            NSCursor.arrow.set()
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            detach()
        } else {
            presentation = ImmersivePresentation(window: window!)
            session?.sessionControl.attach(forwarder: input, window: window)
            updateScale()
            sizeChanged()
        }
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        updateScale()
        sizeChanged()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        sizeChanged()
    }

    func detach() {
        input.detach()
        // The toolbar comes back with whatever the window shows next.
        presentation?.update(immersive: false, fillsScreen: false)
        presentation = nil
        guard attached else { return }
        attached = false
        lk_detach_view(sessionId)
    }

    private func updateScale() {
        metalLayer.contentsScale = window?.backingScaleFactor ?? 2
    }

    private var pixelSize: (UInt32, UInt32) {
        let scale = window?.backingScaleFactor ?? 2
        return (UInt32(max(1, (bounds.width * scale).rounded())), UInt32(max(1, (bounds.height * scale).rounded())))
    }

    private func sizeChanged() {
        guard window != nil, bounds.width > 0, bounds.height > 0 else { return }
        input.pictureMoved()
        let (w, h) = pixelSize
        if attached {
            lk_resize_view(sessionId, w, h)
        } else {
            attached = true
            lk_attach_view(sessionId, Unmanaged.passUnretained(metalLayer).toOpaque(), w, h)
        }
    }
}

/// In full screen while controlling, hides this Mac's menu bar and Dock (and the window's
/// toolbar) so the pointer reaching the top or bottom edge uses the remote Mac's instead. The
/// toolbar also stays hidden in full screen while the session shows a display that fills this
/// screen, so it's shown pixel for pixel.
@MainActor
final class ImmersivePresentation {
    private weak var window: NSWindow?
    private var saved: NSApplication.PresentationOptions?
    private var wanted = false
    private var fillsScreen = false
    private var observers: [NSObjectProtocol] = []

    init(window: NSWindow) {
        self.window = window
        for name in [NSWindow.didEnterFullScreenNotification, NSWindow.willExitFullScreenNotification] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.apply() }
            })
        }
    }

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver)
    }

    func update(immersive: Bool, fillsScreen: Bool? = nil) {
        wanted = immersive
        if let fillsScreen { self.fillsScreen = fillsScreen }
        apply()
    }

    private func apply() {
        guard let window else { return }
        let fullScreen = window.styleMask.contains(.fullScreen) && window.isKeyWindow
        let on = wanted && fullScreen
        window.toolbar?.isVisible = !(fullScreen && (wanted || fillsScreen))
        if on {
            if saved == nil { saved = NSApp.presentationOptions }
            // hideMenuBar needs hideDock; autoHideToolbar is not allowed with hideMenuBar.
            NSApp.presentationOptions = [.fullScreen, .hideDock, .hideMenuBar, .disableHideApplication]
        } else if let saved {
            NSApp.presentationOptions = saved
            self.saved = nil
        }
    }
}
