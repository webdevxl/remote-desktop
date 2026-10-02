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
                RemoteScreen(sessionId: session.id)
                    .background(Color.black)
                    .overlay(alignment: .topLeading) {
                        if showStats {
                            StatsHUD(sessionId: session.id, info: info).padding(12)
                        }
                    }
            case .ended(let error):
                EndedView(target: session.target, error: error, wasConnected: session.wasConnected,
                          reconnect: { reconnect(session.target) }, close: close)
            }
        }
        .navigationTitle(title)
        .toolbar {
            if case .connected = session.phase {
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

/// Hosts the CAMetalLayer the Rust core renders into, on its own thread.
private struct RemoteScreen: NSViewRepresentable {
    let sessionId: UInt64

    func makeNSView(context: Context) -> MetalHostView {
        MetalHostView(sessionId: sessionId)
    }

    func updateNSView(_ view: MetalHostView, context: Context) {}

    static func dismantleNSView(_ view: MetalHostView, coordinator: ()) {
        view.detach()
    }
}

final class MetalHostView: NSView {
    let sessionId: UInt64
    private let metalLayer = CAMetalLayer()
    private var attached = false

    init(sessionId: UInt64) {
        self.sessionId = sessionId
        super.init(frame: .zero)
        metalLayer.isOpaque = true
        metalLayer.backgroundColor = NSColor.black.cgColor
        wantsLayer = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func makeBackingLayer() -> CALayer { metalLayer }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            detach()
        } else {
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
