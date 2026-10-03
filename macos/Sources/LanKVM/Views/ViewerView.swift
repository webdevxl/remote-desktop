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
                ViewerContent(session: session, reconnect: reconnect, close: { dismiss() })
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
                RemoteScreen(session: session, info: info, toggleControl: toggleControl)
                    .background(Color.black)
                    .overlay(alignment: .topLeading) {
                        if showStats {
                            // Never in the way of the remote Mac's menu bar.
                            StatsHUD(sessionId: session.id, info: info).padding(12).allowsHitTesting(false)
                        }
                    }
                    .overlay(alignment: .top) {
                        ControlBanner(session: session, hostName: info.hostName,
                                      retry: { core.setControl(true, for: session.id) },
                                      takeOver: { core.setControl(true, for: session.id, takeOver: true) })
                            .padding(.top, 14)
                    }
                    .overlay(alignment: .bottom) {
                        if let toast = session.toast {
                            Text(toast)
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
            case .ended(let error):
                EndedView(target: session.target, error: error, wasConnected: session.wasConnected,
                          reconnect: { reconnect(session.target) }, close: close)
            }
        }
        .navigationTitle(title)
        .navigationSubtitle(subtitle)
        .toolbar {
            if case .connected = session.phase {
                ToolbarItem(placement: .principal) {
                    Picker("Mode", selection: Binding(get: { session.mode }, set: { setMode($0) })) {
                        Label("View", systemImage: "eye").tag(SessionModel.Mode.view)
                        Label("Control", systemImage: "cursorarrow.rays").tag(SessionModel.Mode.control)
                    }
                    .pickerStyle(.segmented)
                    .labelStyle(.titleAndIcon)
                    .help("View only, or control this Mac with your mouse and keyboard. Press ⌃⌥⌘ together to switch.")
                }
                ToolbarItemGroup(placement: .primaryAction) {
                    Toggle(isOn: $showStats) {
                        Label("Statistics", systemImage: "gauge.with.dots.needle.33percent")
                    }
                    .help("Show latency and bandwidth")
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
        switch session.control {
        case .active where session.sameMachine: return "Controlling this Mac (test mode) · press ⌃⌥⌘ to stop"
        case .active: return "Controlling · press ⌃⌥⌘ to stop"
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
                pill(icon: "exclamationmark.triangle", text: "Controlling this same Mac (test mode). Pointer and keyboard are shared; press ⌃⌥⌘ to stop.",
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
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: icon).foregroundStyle(tint)
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
    let cancel: () -> Void

    var body: some View {
        ZStack {
            Color.lkBackground
            VStack(spacing: 14) {
                ProgressView().controlSize(.large)
                Text("Connecting to \(target)…")
                    .font(.system(size: 15, design: .serif))
                    .foregroundStyle(Color.lkText)
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
                Text(verbatim: "\(String(info.width))×\(String(info.height)) · \(info.codec.uppercased())")
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
    let toggleControl: () -> Void

    func makeNSView(context: Context) -> MetalHostView {
        MetalHostView(sessionId: session.id)
    }

    func updateNSView(_ view: MetalHostView, context: Context) {
        view.configure(session: session, frameSize: CGSize(width: Int(info.width), height: Int(info.height)), toggleControl: toggleControl)
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

    /// Called whenever SwiftUI updates the view (mode or control state changed).
    func configure(session: SessionModel, frameSize: CGSize, toggleControl: @escaping () -> Void) {
        if self.session !== session {
            self.session = session
            input.attach(to: session)
            session.cursor.onChange = { [weak self] in self?.showRemoteCursor() }
            // Full screen hides this Mac's menu bar and Dock exactly while input goes remote,
            // however forwarding starts or stops (focus, sleep, another Space...).
            input.onForwardingChanged = { [weak self] on in self?.presentation?.update(immersive: on) }
        }
        input.frameSize = frameSize
        input.onEscape = toggleControl
        input.update()
        presentation?.update(immersive: input.isForwarding)
        window?.invalidateCursorRects(for: self)
        showRemoteCursor()
    }

    private var controlling: Bool { session?.isControlling == true }

    override var acceptsFirstResponder: Bool { true }
    override var isOpaque: Bool { true }
    // The click that brings the window to the front stays here; the next one goes remote.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { false }
    // A drag on the remote screen must never move this window.
    override var mouseDownCanMoveWindow: Bool { false }

    // MARK: Mouse (keys go through InputForwarder's event monitor)

    override func mouseMoved(with event: NSEvent) {
        guard controlling else { return super.mouseMoved(with: event) }
        input.mouseMoved(event)
        // Hidden while the remote user types: show it again as soon as the mouse moves (hidden
        // again if the host doesn't confirm).
        if session?.cursor.state == .hidden { session?.cursor.unhide() }
        showRemoteCursor(at: convert(event.locationInWindow, from: nil))
    }
    override func mouseDragged(with event: NSEvent) { controlling ? input.mouseMoved(event) : super.mouseDragged(with: event) }
    override func rightMouseDragged(with event: NSEvent) { controlling ? input.mouseMoved(event) : super.rightMouseDragged(with: event) }
    override func otherMouseDragged(with event: NSEvent) { controlling ? input.mouseMoved(event) : super.otherMouseDragged(with: event) }
    override func mouseDown(with event: NSEvent) { controlling ? input.mouseButton(event, down: true) : super.mouseDown(with: event) }
    override func mouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.mouseUp(with: event) }
    override func rightMouseDown(with event: NSEvent) { controlling ? input.mouseButton(event, down: true) : super.rightMouseDown(with: event) }
    override func rightMouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.rightMouseUp(with: event) }
    override func otherMouseDown(with event: NSEvent) { controlling ? input.mouseButton(event, down: true) : super.otherMouseDown(with: event) }
    override func otherMouseUp(with event: NSEvent) { controlling ? input.mouseButton(event, down: false) : super.otherMouseUp(with: event) }
    override func scrollWheel(with event: NSEvent) { controlling ? input.scroll(event) : super.scrollWheel(with: event) }
    // Gestures can't be reproduced on the remote Mac; don't let them act here either.
    override func magnify(with event: NSEvent) { if !controlling { super.magnify(with: event) } }
    override func rotate(with event: NSEvent) { if !controlling { super.rotate(with: event) } }
    override func smartMagnify(with event: NSEvent) { if !controlling { super.smartMagnify(with: event) } }
    override func swipe(with event: NSEvent) { if !controlling { super.swipe(with: event) } }
    // Ctrl-click is a click with Control held, for the remote Mac to interpret.
    override func menu(for event: NSEvent) -> NSMenu? { controlling ? nil : super.menu(for: event) }

    // MARK: Cursor

    override func cursorUpdate(with event: NSEvent) {
        showRemoteCursor(at: convert(event.locationInWindow, from: nil))
    }

    /// Over the picture while controlling: the remote Mac's cursor. Elsewhere: the arrow.
    private func showRemoteCursor(at point: NSPoint? = nil) {
        guard let window, window.isKeyWindow else { return }
        let p = point ?? convert(window.mouseLocationOutsideOfEventStream, from: nil)
        guard bounds.contains(p) else { return }
        if controlling, let cursor = session?.cursor.current, input.pictureRect()?.contains(p) ?? true {
            if NSCursor.current !== cursor { cursor.set() }
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
        presentation?.update(immersive: false)
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
/// toolbar) so the pointer reaching the top or bottom edge uses the remote Mac's instead.
@MainActor
final class ImmersivePresentation {
    private weak var window: NSWindow?
    private var saved: NSApplication.PresentationOptions?
    private var wanted = false
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

    func update(immersive: Bool) {
        wanted = immersive
        apply()
    }

    private func apply() {
        guard let window else { return }
        let fullScreen = window.styleMask.contains(.fullScreen) && window.isKeyWindow
        let on = wanted && fullScreen
        window.toolbar?.isVisible = !on
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
